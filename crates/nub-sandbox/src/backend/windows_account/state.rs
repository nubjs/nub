//! Provisioning marker + the ACL ledger — the only durable state the account backend keeps.
//!
//! TWO FILES, TWO JOBS. The **marker** records what the elevated setup created (the account's
//! SID and the WFP-authorized loopback port window) so an UNELEVATED run can decide whether it
//! may proceed and which port to bind — WFP gates even *enumeration* on administrator, so an
//! unelevated process cannot ask the firewall what is installed and must read it here.
//! The **ledger** records every path ever ACL'd for the sandbox SID so a sweep can undo them
//! after a crash.
//!
//! WHY THE LEDGER IS A FLAT PATH LIST AND NOT A RESTORE JOURNAL. MXC's DACL journal captures
//! each path's PRIOR aces before mutating, because its trustee (`ALL APPLICATION PACKAGES`,
//! `Everyone`) can legitimately already hold aces authored by installers or `icacls` — so
//! "undo" there means *restore what was there*. nub's trustee is an account nub itself
//! created, which by construction has no pre-existing ace anywhere. Undo is therefore an
//! unconditional idempotent strip, and the only thing worth remembering is WHERE. That
//! collapses ~600 lines of prior-state capture, per-run files, PID+creation-time liveness and
//! corrupt-file quarantine into a path list. Ordering is still load-bearing: the path is
//! recorded BEFORE the ace is applied, so a crash in between leaves a ledger entry for an ace
//! that was never written — and stripping a path that has no ace is a no-op.
//!
//! Both live under `%PROGRAMDATA%\nub\sandbox`, which the elevated setup locks to SYSTEM,
//! `BUILTIN\Administrators` and the provisioning user (see [`super::acl::lock_to_admins`] —
//! the same directory holds the DPAPI credential, and its DACL is that blob's ONLY boundary).
//! So the provisioning user reads the marker and appends the ledger unelevated, while another
//! standard user on the box reads neither and fails closed with "not provisioned".

// Compiled on the dev host too, so the marker parse + ledger de-duplication are unit-tested
// without a Windows box; only the Windows launch/setup paths actually call the rest.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

/// Bumped when the on-disk shape changes. A marker from a newer nub is refused rather than
/// misread — an unelevated run acting on a misparsed SID would ACL the wrong principal.
/// v2 dropped the account NAME field, so a v1 marker is refused rather than read past.
pub(crate) const MARKER_VERSION: u32 = 2;

/// What the one-time elevated setup recorded.
///
/// DELIBERATELY CARRIES NO ACCOUNT NAME. The identity is the compile-time
/// [`super::SANDBOX_ACCOUNT`] const, and the launch validates the SID below against a live
/// lookup of THAT const. A name read from disk would have been an attacker-chosen logon target
/// that still passed the SID check — nothing about the identity comes off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    pub(crate) version: u32,
    /// The account's SID string. Every WFP filter and every granted ace keys on this, so a
    /// marker whose SID no longer resolves means the account was deleted out from under us.
    pub(crate) sid: String,
    /// The loopback window the installed WFP permit covers. The egress proxy must bind
    /// inside it or the child cannot reach the proxy at all.
    pub(crate) port_low: u16,
    pub(crate) port_high: u16,
}

impl Marker {
    /// Hand-rolled rather than derived: `serde_json` is a dependency, but a four-field record
    /// does not earn a `Serialize` impl and the parse is the only place a malformed marker can
    /// hurt us, so it stays explicit and total.
    pub(crate) fn to_json(&self) -> String {
        format!(
            "{{\"version\":{},\"sid\":{},\"port_low\":{},\"port_high\":{}}}\n",
            self.version,
            json_string(&self.sid),
            self.port_low,
            self.port_high
        )
    }

    pub(crate) fn from_json(s: &str) -> io::Result<Self> {
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sandbox marker is not valid JSON: {e}"),
            )
        })?;
        let field = |k: &str| -> io::Result<&serde_json::Value> {
            v.get(k).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("sandbox marker is missing `{k}`"),
                )
            })
        };
        let version = field("version")?.as_u64().unwrap_or(0) as u32;
        if version != MARKER_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sandbox marker is version {version}, this nub understands {MARKER_VERSION} \
                     — re-run `nub run --sandbox-admin setup` from an elevated prompt"
                ),
            ));
        }
        let as_str = |v: &serde_json::Value, k: &str| -> io::Result<String> {
            v.as_str().map(str::to_owned).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("sandbox marker field `{k}` is not a string"),
                )
            })
        };
        let as_port = |v: &serde_json::Value, k: &str| -> io::Result<u16> {
            v.as_u64()
                .filter(|n| *n > 0 && *n <= u16::MAX as u64)
                .map(|n| n as u16)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("sandbox marker field `{k}` is not a valid port"),
                    )
                })
        };
        let m = Marker {
            version,
            sid: as_str(field("sid")?, "sid")?,
            port_low: as_port(field("port_low")?, "port_low")?,
            port_high: as_port(field("port_high")?, "port_high")?,
        };
        if m.port_high < m.port_low {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sandbox marker port range is inverted",
            ));
        }
        Ok(m)
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `%PROGRAMDATA%\nub\sandbox`. Machine-scoped on purpose: the account, the WFP filters and
/// the ledger are all machine state, not per-user state.
///
/// THE ONE DEFINITION OF THIS PATH — [`super::account::credential_dir`] delegates here. The
/// credential, the marker and the ledger all live in this directory, and setup locks it by
/// asking for the CREDENTIAL directory, so a second definition that ever drifted would silently
/// protect a directory the credential is not in.
pub(crate) fn state_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("PROGRAMDATA")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "PROGRAMDATA is not set — cannot locate the nub sandbox state directory",
            )
        })?;
    Ok(PathBuf::from(base).join("nub").join("sandbox"))
}

pub(crate) fn marker_path() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("setup.json"))
}

pub(crate) fn ledger_path() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("acl-ledger.txt"))
}

/// ELEVATED (the directory's DACL permits only administrators to write).
pub(crate) fn write_marker(m: &Marker) -> io::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;
    // tmp + rename so a torn write can never leave a half-parsed marker that an unelevated
    // run would act on.
    let path = marker_path()?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, m.to_json())?;
    std::fs::rename(&tmp, &path)
}

/// UNELEVATED. `None` when the machine has never been set up.
pub(crate) fn read_marker() -> io::Result<Option<Marker>> {
    let path = marker_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => Marker::from_json(&s).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn remove_marker() -> io::Result<()> {
    match std::fs::remove_file(marker_path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Record a path BEFORE its ace is applied. Append-only and best-effort-atomic: a single
/// short append under `O_APPEND` semantics is not interleaved by a concurrent writer in
/// practice, and the failure mode of a torn line is a path that is never swept — which the
/// caller's own teardown still handles on the happy path.
pub(crate) fn record_acl_path(path: &Path) -> io::Result<()> {
    use std::io::Write;
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path()?)?;
    writeln!(f, "{}", path.display())
}

/// Every path the ledger has ever recorded, de-duplicated in first-seen order.
pub(crate) fn ledger_paths() -> io::Result<Vec<PathBuf>> {
    let text = match std::fs::read_to_string(ledger_path()?) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(dedup_lines(&text))
}

/// Split out so the de-duplication is testable without a filesystem.
fn dedup_lines(text: &str) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let p = PathBuf::from(line);
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen
}

/// Called only after every recorded path has been stripped.
pub(crate) fn clear_ledger() -> io::Result<()> {
    match std::fs::remove_file(ledger_path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Marker {
        Marker {
            version: MARKER_VERSION,
            sid: "S-1-5-21-1-2-3-1001".into(),
            port_low: 59080,
            port_high: 59089,
        }
    }

    #[test]
    fn marker_round_trips() {
        let m = sample();
        assert_eq!(Marker::from_json(&m.to_json()).unwrap(), m);
    }

    /// A marker written by a NEWER nub must be refused, not partially believed: acting on a
    /// misread SID would ACL the wrong principal, and acting on a misread port range would
    /// bind the proxy outside the WFP permit so the child silently reaches nothing.
    #[test]
    fn a_future_marker_version_is_refused() {
        let json = format!(
            "{{\"version\":{},\"sid\":\"S-1-5-21-1\",\"port_low\":1,\"port_high\":2}}",
            MARKER_VERSION + 1
        );
        let err = Marker::from_json(&json).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("--sandbox-admin setup"));
    }

    /// The v1 marker carried the account NAME, and the launch logged on as it. Anyone able to
    /// write the marker file could keep the real SID (so the SID check still passed) while
    /// pointing the name at an account they controlled — every "sandboxed" run then launched
    /// unfenced. v2 removed the field, so a v1 marker must be REFUSED outright rather than
    /// parsed with its name ignored.
    #[test]
    fn a_v1_marker_carrying_an_account_name_is_refused() {
        let json = "{\"version\":1,\"account\":\"attacker\",\"sid\":\"S-1-5-21-1\",\
                    \"port_low\":1,\"port_high\":2}";
        let err = Marker::from_json(json).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// An inverted or zero range would make the proxy's bind-in-range search fail in a way
    /// that reads as "no free port" rather than "your setup is corrupt".
    #[test]
    fn a_malformed_port_range_is_refused() {
        for (low, high) in [(900, 800), (0, 800)] {
            let json = format!(
                "{{\"version\":{MARKER_VERSION},\"sid\":\"S\",\"port_low\":{low},\"port_high\":{high}}}"
            );
            assert!(Marker::from_json(&json).is_err(), "{json}");
        }
    }

    /// The ledger is append-only, so the same path recurs across runs; the sweep must visit
    /// each path once rather than re-stripping it N times.
    #[test]
    fn ledger_deduplicates_repeated_paths() {
        let paths = dedup_lines("C:/a\nC:/b\nC:/a\n\n  C:/c  \n");
        assert_eq!(
            paths,
            vec![
                PathBuf::from("C:/a"),
                PathBuf::from("C:/b"),
                PathBuf::from("C:/c")
            ]
        );
    }
}
