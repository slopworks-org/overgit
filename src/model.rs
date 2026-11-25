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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub opaque: BTreeSet<String>,
}
impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 2,
            entries: BTreeMap::new(),
            opaque: BTreeSet::new(),
        }
    }
}
impl Manifest {
    pub fn opaque_directory(&self, path: &str) -> Option<&str> {
        self.opaque.get(path).map(String::as_str).or_else(|| {
            parents(path).find_map(|parent| self.opaque.get(parent).map(String::as_str))
        })
    }

    /// Called for tracked paths. Untracked files only belong to the overlay after add.
    pub fn manages_tracked(&self, path: &str) -> bool {
        self.entries.contains_key(path) || self.opaque_directory(path).is_some()
    }

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
        let mut directories = BTreeMap::new();
        for dir in &self.opaque {
            validate_path(dir)?;
            let identity = path_identity(dir);
            ensure!(
                !folded.contains(&identity) && directories.insert(identity, dir).is_none(),
                "colliding opaque directory {dir:?}"
            );
        }
        for path in self.entries.keys().chain(&self.opaque) {
            for parent in parents(path) {
                let identity = path_identity(parent);
                ensure!(
                    !folded.contains(&identity),
                    "overlapping manifest paths {path:?} and {parent:?}"
                );
                if let Some(dir) = directories.get(&identity) {
                    ensure!(
                        *dir == parent,
                        "ambiguous opaque directory spelling at {path:?}; use {dir:?}"
                    );
                    ensure!(
                        self.entries
                            .get(path)
                            .is_some_and(|e| e.kind != Kind::Delete),
                        "redundant whiteout or opaque directory {path:?} inside {dir:?}"
                    );
                }
            }
        }
        Ok(())
    }
}

pub fn parents(path: &str) -> impl DoubleEndedIterator<Item = &str> {
    path.match_indices('/').map(|(end, _)| &path[..end])
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
