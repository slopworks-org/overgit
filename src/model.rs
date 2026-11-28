use anyhow::{Context as _, Result, bail, ensure};
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
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub patterns: BTreeSet<String>,
}
impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 2,
            entries: BTreeMap::new(),
            opaque: BTreeSet::new(),
            patterns: BTreeSet::new(),
        }
    }
}
impl Manifest {
    pub fn opaque_directory(&self, path: &str) -> Option<&str> {
        self.opaque.get(path).map(String::as_str).or_else(|| {
            parents(path).find_map(|parent| self.opaque.get(parent).map(String::as_str))
        })
    }

    pub fn matching_pattern(&self, path: &str) -> Option<&str> {
        self.patterns
            .iter()
            .find(|pattern| matches_pattern(pattern, path))
            .map(String::as_str)
    }

    pub fn has_masks(&self) -> bool {
        !self.opaque.is_empty() || !self.patterns.is_empty()
    }

    pub fn is_masked(&self, path: &str) -> bool {
        self.opaque_directory(path).is_some() || self.matching_pattern(path).is_some()
    }

    /// Called for tracked paths. Untracked files only belong to the overlay after add.
    pub fn manages_tracked(&self, path: &str) -> bool {
        self.entries.contains_key(path) || self.is_masked(path)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2,
            "unsupported manifest version {}; expected 2",
            self.version
        );
        for pattern in &self.patterns {
            validate_pattern(pattern)?;
        }
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
            if entry.kind == Kind::Delete
                && let Some(pattern) = self.matching_pattern(path)
            {
                bail!("redundant whiteout {path:?} matches pattern {pattern:?}");
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

enum Token {
    Lit(char),
    Any,
    Star,
    Class(bool, Vec<(char, char)>),
}

fn next_class_char(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<char> {
    match chars.next() {
        Some('\\') => chars.next().context("trailing escape in pattern"),
        Some(ch) => Ok(ch),
        None => bail!("unclosed character class in pattern"),
    }
}

fn parse_segment(segment: &str) -> Result<Vec<Token>> {
    let mut chars = segment.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if !matches!(tokens.last(), Some(Token::Star)) => tokens.push(Token::Star),
            '*' => {}
            '?' => tokens.push(Token::Any),
            '\\' => tokens.push(Token::Lit(
                chars.next().context("trailing escape in pattern")?,
            )),
            '[' => {
                let negated = chars.next_if(|c| matches!(c, '!' | '^')).is_some();
                let mut ranges = Vec::new();
                while chars.peek() != Some(&']') || ranges.is_empty() {
                    let lo = next_class_char(&mut chars)?;
                    if chars.next_if_eq(&'-').is_some() {
                        if chars.peek() == Some(&']') {
                            ranges.extend([(lo, lo), ('-', '-')]);
                            break;
                        }
                        let hi = next_class_char(&mut chars)?;
                        ensure!(lo <= hi, "reversed range {lo:?}-{hi:?} in pattern");
                        ranges.push((lo, hi));
                    } else {
                        ranges.push((lo, lo));
                    }
                }
                ensure!(
                    chars.next() == Some(']'),
                    "unclosed character class in pattern"
                );
                tokens.push(Token::Class(negated, ranges));
            }
            other => tokens.push(Token::Lit(other)),
        }
    }
    Ok(tokens)
}

fn match_seq<P, T>(
    pat: &[P],
    items: &[T],
    is_star: impl Fn(&P) -> bool,
    eq: impl Fn(&P, &T) -> bool,
    trailing_nonempty: bool,
) -> bool {
    let (mut pi, mut ti, mut star_p, mut star_t) = (0, 0, None, 0);
    while ti < items.len() {
        if let Some(p) = pat.get(pi)
            && is_star(p)
        {
            if trailing_nonempty && pi + 1 == pat.len() {
                return true;
            }
            (star_p, star_t, pi) = (Some(pi), ti, pi + 1);
        } else if let Some(p) = pat.get(pi)
            && eq(p, &items[ti])
        {
            (pi, ti) = (pi + 1, ti + 1);
        } else if let Some(back) = star_p {
            star_t += 1;
            (ti, pi) = (star_t, back + 1);
        } else {
            return false;
        }
    }
    while pat.get(pi).is_some_and(&is_star) && (!trailing_nonempty || pi + 1 < pat.len()) {
        pi += 1;
    }
    pi == pat.len()
}

fn match_segment(segment: &str, text: &str) -> bool {
    let Ok(tokens) = parse_segment(segment) else {
        return false;
    };
    let chars: Vec<char> = text.chars().collect();
    match_seq(
        &tokens,
        &chars,
        |t| matches!(t, Token::Star),
        |t, &c| match t {
            Token::Any => true,
            Token::Lit(expected) => *expected == c,
            Token::Class(negated, ranges) => {
                ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *negated
            }
            Token::Star => false,
        },
        false,
    )
}

pub fn validate_pattern(pattern: &str) -> Result<()> {
    ensure!(
        !pattern.is_empty() && !pattern.contains(['\0', '\n', '\r']),
        "invalid empty or control-character pattern {pattern:?}"
    );
    ensure!(
        !pattern.starts_with('!'),
        "negation patterns are unsupported at {pattern:?}; use `overgit add` to keep a specific file"
    );
    ensure!(
        !pattern.ends_with('/'),
        "trailing '/' is unsupported in pattern {pattern:?}; use `overgit rm <dir>` or `/**`"
    );
    let body = pattern.strip_prefix('/').unwrap_or(pattern);
    ensure!(!body.is_empty(), "invalid empty pattern {pattern:?}");
    for (index, segment) in body.split('/').enumerate() {
        ensure!(
            !segment.is_empty(),
            "invalid path segment in pattern {pattern:?}"
        );
        if segment == "**" {
            continue;
        }
        let tokens = parse_segment(segment)?;
        if tokens.iter().all(|token| matches!(token, Token::Lit(_))) {
            let literal: String = tokens
                .iter()
                .filter_map(|token| match token {
                    Token::Lit(ch) => Some(*ch),
                    _ => None,
                })
                .collect();
            ensure!(
                !matches!(literal.as_str(), "" | "." | ".."),
                "invalid path segment in pattern {pattern:?}"
            );
            if literal.eq_ignore_ascii_case(".git")
                || (index == 0 && literal.eq_ignore_ascii_case(".overgit"))
            {
                bail!("reserved pattern {pattern:?}");
            }
        }
    }
    Ok(())
}

pub fn matches_pattern(pattern: &str, path: &str) -> bool {
    let Some(body) = pattern
        .strip_prefix('/')
        .or_else(|| pattern.contains('/').then_some(pattern))
    else {
        return match_segment(pattern, path.rsplit('/').next().unwrap_or(path));
    };
    let rules: Vec<&str> = body.split('/').collect();
    let parts: Vec<&str> = path.split('/').collect();
    match_seq(
        &rules,
        &parts,
        |r| *r == "**",
        |r, p| match_segment(r, p),
        true,
    )
}
