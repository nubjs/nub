use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
pub(crate) struct Seed {
    #[serde(default, rename = "e")]
    pub(crate) etag: Option<String>,
    #[serde(default, rename = "lm")]
    pub(crate) last_modified: Option<String>,
    #[serde(rename = "p")]
    pub(super) packument: PrimerPackument,
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
pub(super) struct PrimerPackument {
    #[serde(rename = "n")]
    pub(super) name: String,
    #[serde(default, rename = "m")]
    pub(super) modified: Option<String>,
    #[serde(default, rename = "d")]
    pub(super) dist_tags: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "v")]
    pub(super) versions: Vec<PrimerVersion>,
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
pub(super) struct PrimerVersion {
    #[serde(rename = "v")]
    pub(super) version: String,
    #[serde(default, rename = "t")]
    pub(super) published_at: Option<String>,
    #[serde(default, rename = "m")]
    pub(super) metadata: PrimerVersionMetadata,
}

#[derive(Archive, Clone, Default, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
pub(super) struct PrimerVersionMetadata {
    #[serde(default, rename = "d")]
    pub(super) dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "p")]
    pub(super) peer_dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "pm")]
    pub(super) peer_dependencies_meta: std::collections::BTreeMap<String, PrimerPeerDepMeta>,
    #[serde(default, rename = "o")]
    pub(super) optional_dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "b")]
    pub(super) bundled_dependencies: Option<PrimerBundledDependencies>,
    #[serde(default, rename = "dt")]
    pub(super) dist: Option<PrimerDist>,
    #[serde(default)]
    pub(super) os: Vec<String>,
    #[serde(default)]
    pub(super) cpu: Vec<String>,
    #[serde(default)]
    pub(super) libc: Vec<String>,
    #[serde(default, rename = "e")]
    pub(super) engines: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "l")]
    pub(super) license: Option<String>,
    #[serde(default, rename = "f")]
    pub(super) funding_url: Option<String>,
    #[serde(default)]
    pub(super) bin: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "h")]
    pub(super) has_install_script: bool,
    #[serde(default, rename = "x")]
    pub(super) deprecated: Option<String>,
    #[serde(default, rename = "u")]
    pub(super) trusted_publisher: bool,
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
pub(super) struct PrimerPeerDepMeta {
    #[serde(default)]
    pub(super) optional: bool,
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
#[serde(untagged)]
pub(super) enum PrimerBundledDependencies {
    List(Vec<String>),
    All(bool),
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize, serde::Deserialize)]
pub(super) struct PrimerDist {
    /// `None` for npm publishes whose tarball URL matches the
    /// deterministic `{registry}/{name}/-/{unscoped}-{version}.tgz`
    /// pattern (the generator omits the field). Carried explicitly
    /// only for the legacy outliers (e.g. `handlebars@1.0.2-beta`
    /// publishes as `handlebars-1.0.2beta.tgz`) that diverge.
    #[serde(default, rename = "t")]
    pub(super) tarball: Option<String>,
    #[serde(default, rename = "i")]
    pub(super) integrity: Option<PrimerIntegrity>,
    #[serde(default, rename = "a")]
    pub(super) provenance: bool,
}

/// An SRI integrity string, stored as the raw digest. This is the primer's
/// single largest field — one `sha512-<base64>` per version, ~97k of them in
/// the release primer — and a digest compresses no better than its own bytes,
/// so the base64 text costs a third more for nothing. Re-encoded on read.
#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
pub(super) enum PrimerIntegrity {
    Sha512([u8; 64]),
    /// Any other SRI form (the ~100 legacy `sha1-` publishes), verbatim.
    Other(String),
}

impl PrimerIntegrity {
    pub(super) fn from_sri(sri: String) -> Self {
        use base64::Engine as _;
        if let Some(b64) = sri.strip_prefix("sha512-")
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
            && let Ok(digest) = <[u8; 64]>::try_from(bytes)
        {
            return Self::Sha512(digest);
        }
        Self::Other(sri)
    }

    pub(super) fn to_sri(&self) -> String {
        use base64::Engine as _;
        match self {
            Self::Sha512(digest) => format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode(digest)
            ),
            Self::Other(sri) => sri.clone(),
        }
    }
}

impl<'de> serde::Deserialize<'de> for PrimerIntegrity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from_sri)
    }
}
