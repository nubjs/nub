//! The ephemeral MITM certificate authority for the credential-brokering tier.
//!
//! SECURITY POSTURE (proposal §5 + the U5 dispatch requirements) — the invariants this
//! module exists to hold:
//!
//! - **Per-run + ephemeral.** The CA is minted when the proxy starts and gone when it
//!   drops. Nothing survives the run; no cross-run artifact exists.
//! - **The CA private key NEVER leaves this process's memory.** It is held only in
//!   [`MitmCa::ca_key`] and used only to sign leaves in-process — it is NEVER written to
//!   disk (stronger than SRT, which writes the key to a temp dir). Only the CA
//!   CERTIFICATE (public) is emitted.
//! - **The OS trust store is NEVER touched.** No `security add-trusted-cert`, no
//!   `/etc/ssl` write. Trust reaches the child ONLY through the constructed child env: a
//!   CA-bundle file the CA-env vars point at (see `backend::set_ca_env`), scoped to the
//!   child, invisible to every other process, removed when this value drops.
//! - **The bundle is CA cert + the platform's REAL roots**, never CA-alone — the
//!   `SSL_CERT_FILE`-class vars REPLACE a tool's store, so a CA-only file would break
//!   verification of every blind-tunneled (non-terminated) host.
//!
//! FAIL-CLOSED: every minting/IO failure is an `io::Error` the caller turns into a
//! denied connection — there is no plaintext fallback anywhere on this path.

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::{fs::File, os::fd::FromRawFd, os::fd::RawFd};

/// The per-run ephemeral CA plus the child trust bundle it anchors.
pub(super) struct MitmCa {
    ca_cert: Certificate,
    ca_key: KeyPair,
    /// The platform's real roots (DER), retained for the proxy's OUTBOUND leg — the
    /// upstream connection verifies the real server cert against these, so nub itself is
    /// never MITM'd. Also PEM-encoded into the child bundle.
    native_roots: Vec<CertificateDer<'static>>,
    /// The child-scoped CA-bundle file (CA cert + real roots). The `NamedTempFile` owns
    /// the file's lifetime: 0600 on Unix (mkstemp), removed on drop. Holds ONLY public
    /// certs — never the CA key.
    _bundle: tempfile::NamedTempFile,
    bundle_path: PathBuf,
    /// Linux launches consume an immutable descriptor instead of reopening the
    /// same-user-writable temporary pathname during Bubblewrap setup.
    #[cfg(target_os = "linux")]
    bundle_file: File,
}

impl MitmCa {
    /// Mint the ephemeral CA and write the child trust bundle. Fail-closed: an error here
    /// aborts engaging the tier (the caller degrades / denies), never a silent downgrade.
    pub(super) fn generate() -> io::Result<MitmCa> {
        let ca_key = KeyPair::generate().map_err(mint_err)?;
        // A minimal CA: cert-signing key usage, unconstrained basic-constraints. Its only
        // job is to sign the per-host leaves this same process presents to the child.
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(mint_err)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "nub sandbox ephemeral CA");
        let ca_cert = params.self_signed(&ca_key).map_err(mint_err)?;

        // Real platform roots. Empty ⇒ fail-closed: without them the child cannot verify
        // blind-tunneled hosts and the proxy cannot verify upstreams.
        let native_roots = rustls_native_certs::load_native_certs().certs;
        if native_roots.is_empty() {
            return Err(io::Error::other(
                "no platform root certificates could be loaded for the MITM trust bundle",
            ));
        }

        let bytes = bundle_bytes(&ca_cert, &native_roots);
        let bundle = write_bundle(&bytes)?;
        let bundle_path = bundle.path().to_path_buf();
        #[cfg(target_os = "linux")]
        let bundle_file = sealed_bundle_file(&bytes)?;
        Ok(MitmCa {
            ca_cert,
            ca_key,
            native_roots,
            _bundle: bundle,
            bundle_path,
            #[cfg(target_os = "linux")]
            bundle_file,
        })
    }

    /// The child-scoped CA-bundle path (what the CA-env vars point at).
    pub(super) fn bundle_path(&self) -> &Path {
        &self.bundle_path
    }

    #[cfg(target_os = "linux")]
    pub(super) fn bundle_file(&self) -> io::Result<File> {
        self.bundle_file.try_clone()
    }

    /// The real platform roots — the proxy's upstream leg verifies against these.
    pub(super) fn native_roots(&self) -> &[CertificateDer<'static>] {
        &self.native_roots
    }

    /// Mint a leaf cert for `host`, signed by the ephemeral CA. Fresh per call (cut-1
    /// mints per terminated connection — a per-host cache is a perf follow-up, not a
    /// correctness one). The returned chain is leaf-only: the child trusts the CA
    /// directly (via the bundle), so the leaf→CA link is verified against that anchor.
    pub(super) fn leaf_for(
        &self,
        host: &str,
    ) -> io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let leaf_key = KeyPair::generate().map_err(mint_err)?;
        let mut params = CertificateParams::new(vec![host.to_string()]).map_err(mint_err)?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.distinguished_name.push(DnType::CommonName, host);
        let leaf = params
            .signed_by(&leaf_key, &self.ca_cert, &self.ca_key)
            .map_err(mint_err)?;
        let chain = vec![leaf.der().clone()];
        // rcgen serializes the private key as PKCS#8 DER.
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        Ok((chain, key))
    }
}

/// Write the child trust bundle (CA cert + real roots, all PUBLIC) to a temp file the
/// `NamedTempFile` owns. On Unix the file is 0600 (mkstemp); it is removed on drop.
fn bundle_bytes(ca_cert: &Certificate, roots: &[CertificateDer<'static>]) -> Vec<u8> {
    let mut bytes = ca_cert.pem().into_bytes();
    bytes.push(b'\n');
    for der in roots {
        let block = pem::encode(&pem::Pem::new("CERTIFICATE", der.as_ref().to_vec()));
        bytes.extend_from_slice(block.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn write_bundle(bytes: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let mut f = tempfile::Builder::new()
        .prefix("nub-mitm-ca-")
        .suffix(".pem")
        .tempfile()?;
    f.write_all(bytes)?;
    f.flush()?;
    Ok(f)
}

#[cfg(target_os = "linux")]
fn sealed_bundle_file(bytes: &[u8]) -> io::Result<File> {
    use std::io::{Seek, Write};
    use std::os::fd::AsRawFd;

    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"nub-ca-bundle".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as RawFd
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.rewind()?;
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, required) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn mint_err(e: rcgen::Error) -> io::Error {
    io::Error::other(format!("MITM certificate minting failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_bundle_holds_public_certs_and_never_the_key() {
        let ca = MitmCa::generate().expect("CA generates on a host with platform roots");
        let bundle = std::fs::read_to_string(ca.bundle_path()).expect("bundle readable");
        // The bundle is CA cert + real roots — multiple CERTIFICATE blocks, at least one
        // per the CA plus the platform store — and NEVER a PRIVATE KEY block.
        assert!(
            bundle.contains("-----BEGIN CERTIFICATE-----"),
            "bundle must carry the CA certificate"
        );
        assert!(
            !bundle.contains("PRIVATE KEY"),
            "the CA private key must NEVER be written to disk"
        );
        assert!(
            bundle.matches("-----BEGIN CERTIFICATE-----").count() >= 2,
            "bundle must include the platform roots alongside the CA (replace-store safety)"
        );
    }

    #[test]
    fn bundle_file_is_removed_when_the_ca_drops() {
        let path = {
            let ca = MitmCa::generate().expect("CA generates");
            ca.bundle_path().to_path_buf()
        };
        assert!(
            !path.exists(),
            "the ephemeral CA bundle must not outlive the run"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bundle_descriptor_is_sealed_and_matches_the_bundle() {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        let ca = MitmCa::generate().expect("CA generates");
        let mut sealed = ca.bundle_file().expect("sealed descriptor clones");
        let seals = unsafe { libc::fcntl(sealed.as_raw_fd(), libc::F_GET_SEALS) };
        let required =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        assert_eq!(seals & required, required);
        let mut bytes = Vec::new();
        sealed.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, std::fs::read(ca.bundle_path()).unwrap());
    }

    #[test]
    fn mints_a_leaf_for_a_host() {
        let ca = MitmCa::generate().expect("CA generates");
        let (chain, _key) = ca.leaf_for("api.example.com").expect("leaf mints");
        assert_eq!(chain.len(), 1, "leaf-only chain (child anchors on the CA)");
    }
}
