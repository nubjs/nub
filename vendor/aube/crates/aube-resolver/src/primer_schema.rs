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
    pub(super) integrity: Option<String>,
    #[serde(default, rename = "a")]
    pub(super) provenance: bool,
}
