use anyhow::{Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization as _;

pub const MANIFEST: &str = ".overgit/manifest.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub mode: String,
    #[serde(with = "bytes")]
    pub data: Vec<u8>,
}

mod bytes {
    use super::*;
    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(data))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(de)?)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Add,
    Override,
    Delete,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Override => "override",
            Self::Delete => "delete",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: Kind,
    pub base: Option<Snapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub entries: BTreeMap<String, Entry>,
}
impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 2,
            entries: BTreeMap::new(),
        }
    }
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2,
            "unsupported manifest version {}; expected 2",
            self.version
        );
        let mut folded = BTreeSet::new();
        for (path, entry) in &self.entries {
            validate_path(path)?;
            ensure!(
                folded.insert(path_identity(path)),
                "canonically or case-colliding manifest path {path:?}"
            );
            ensure!(
                (entry.kind == Kind::Add) == entry.base.is_none(),
                "invalid fork snapshot for {path:?}"
            );
            if let Some(base) = &entry.base {
                base.validate()?;
            }
        }
        for path in self.entries.keys() {
            let mut ancestor = std::path::Path::new(path).parent();
            while let Some(parent) = ancestor {
                if let Some(parent) = parent.to_str() {
                    ensure!(
                        !folded.contains(&path_identity(parent)),
                        "overlapping manifest paths {path:?} and {parent:?}"
                    );
                }
                ancestor = parent.parent();
            }
        }
        Ok(())
    }
}
impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.mode.as_str(), "100644" | "100755" | "120000"),
            "unsupported file mode {}",
            self.mode
        );
        if self.mode == "120000" {
            ensure!(
                !self.data.is_empty() && !self.data.contains(&0),
                "invalid symlink target"
            );
        }
        Ok(())
    }
}
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains('\0'),
        "invalid empty or NUL path"
    );
    for (index, part) in path.split('/').enumerate() {
        ensure!(!matches!(part, "" | "." | ".."), "invalid path {path:?}");
        if part.eq_ignore_ascii_case(".git")
            || (index == 0 && part.eq_ignore_ascii_case(".overgit"))
        {
            bail!("reserved path {path:?}");
        }
    }
    Ok(())
}

/// Portable ownership identity, without changing the stored path spelling.
pub fn path_identity(path: &str) -> String {
    path.nfc().flat_map(char::to_lowercase).nfc().collect()
}
