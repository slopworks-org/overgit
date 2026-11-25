use crate::{
    git,
    model::{Entry, Kind, MANIFEST, Manifest},
    repo::{Repo, atomic_write},
};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::{ffi::OsStringExt as _, fs::MetadataExt as _},
    path::PathBuf,
};
use unicode_normalization::UnicodeNormalization as _;

const BEGIN: &str = "# overgit begin";
const END: &str = "# overgit end";

pub fn worktree_roots(repo: &Repo) -> Result<Vec<PathBuf>> {
    let bytes = repo.base(["worktree", "list", "--porcelain", "-z"])?;
    let mut roots = Vec::new();
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            roots.push(PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())));
        }
    }
    Ok(roots)
}
fn ignore_pattern(path: &str) -> Result<String> {
    ensure!(
        !path.contains('\n') && !path.ends_with('\r'),
        "added path {path:?} cannot be represented by an exact Git ignore pattern"
    );
    let mut pattern = String::from("/");
    for character in path.chars() {
        if "\\*?[]!# ".contains(character) {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    Ok(pattern)
}
fn assert_reserved_alias_safe(root: &std::path::Path, path: &str, alias: &str) -> Result<()> {
    if path == alias {
        return Ok(());
    }
    let aliased = match fs::symlink_metadata(root.join(alias)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let original = match fs::symlink_metadata(root.join(path)) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        original.is_some_and(
            |metadata| metadata.dev() == aliased.dev() && metadata.ino() == aliased.ino()
        ),
        "cannot reserve normalized alias {alias:?} for {path:?}; a distinct file already exists"
    );
    // Equal inodes can still be distinct hardlink entries. Check the parents and
    // actual directory listing before treating two spellings as the same entry.
    let original_path = root.join(path);
    let alias_path = root.join(alias);
    let parent = original_path.parent().context("path has no parent")?;
    let alias_parent = alias_path.parent().context("alias has no parent")?;
    let parent_metadata = fs::metadata(parent)?;
    let alias_parent_metadata = fs::metadata(alias_parent)?;
    ensure!(
        parent_metadata.dev() == alias_parent_metadata.dev()
            && parent_metadata.ino() == alias_parent_metadata.ino(),
        "cannot reserve distinct normalized alias {alias:?} for {path:?}"
    );
    let name = original_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid filename")?;
    let identity = crate::model::path_identity(name);
    let mut entries = 0;
    for entry in fs::read_dir(parent)? {
        if entry?
            .file_name()
            .to_str()
            .is_some_and(|name| crate::model::path_identity(name) == identity)
        {
            entries += 1;
        }
    }
    ensure!(
        entries == 1,
        "cannot reserve distinct normalized directory entry {alias:?} for {path:?}"
    );
    Ok(())
}

pub fn exclude_bytes(repo: &Repo, desired: Option<&Manifest>, mounted: bool) -> Result<Vec<u8>> {
    let path = repo.common.join("info/exclude");
    crate::repo::ensure_not_symlink(&path)?;
    let old = match fs::read(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut unmanaged = Vec::new();
    let mut managed = false;
    for line in old.split_inclusive(|byte| *byte == b'\n') {
        let marker = line.strip_suffix(b"\n").unwrap_or(line);
        let marker = marker.strip_suffix(b"\r").unwrap_or(marker);
        if marker == BEGIN.as_bytes() {
            managed = true;
        } else if marker == END.as_bytes() {
            managed = false;
        } else if !managed {
            unmanaged.extend_from_slice(line);
        }
    }
    ensure!(
        !managed,
        "unterminated overgit exclude block; repair info/exclude manually"
    );
    let mut additions = BTreeSet::new();
    for root in worktree_roots(repo)? {
        let manifest = if root == repo.root {
            if !mounted {
                continue;
            }
            Some(match desired {
                Some(manifest) => manifest.clone(),
                None => repo.manifest()?,
            })
        } else {
            let local = root.join(".overgit/local/detached.json");
            if local.exists() {
                continue;
            }
            let path = root.join(MANIFEST);
            if !path.exists() {
                continue;
            }
            crate::repo::ensure_not_symlink(&root.join(".overgit"))?;
            crate::repo::ensure_not_symlink(&path)?;
            Some(serde_json::from_slice::<Manifest>(&fs::read(path)?)?)
        };
        if let Some(manifest) = manifest {
            manifest.validate()?;
            let base_index = if root == repo.root {
                repo.index(false)?
            } else {
                Repo::discover(&root)?.index(false)?
            };
            for (path, entry) in manifest.entries {
                let untracked = entry.kind == Kind::Add
                    || (entry.kind != Kind::Delete && !base_index.contains_key(&path));
                for alias in [path.clone(), path.nfc().collect(), path.nfd().collect()] {
                    if untracked || alias != path {
                        assert_reserved_alias_safe(&root, &path, &alias)?;
                        additions.insert(ignore_pattern(&alias)?);
                    }
                }
            }
        }
    }
    if !unmanaged.is_empty() && !unmanaged.ends_with(b"\n") {
        unmanaged.push(b'\n');
    }
    unmanaged.extend_from_slice(BEGIN.as_bytes());
    unmanaged.extend_from_slice(b"\n/.overgit/\n");
    for addition in additions {
        unmanaged.extend_from_slice(addition.as_bytes());
        unmanaged.push(b'\n');
    }
    unmanaged.extend_from_slice(END.as_bytes());
    unmanaged.push(b'\n');
    Ok(unmanaged)
}
pub fn excludes(repo: &Repo) -> Result<bool> {
    let text = exclude_bytes(repo, None, !repo.detached())?;
    let path = repo.common.join("info/exclude");
    if fs::read(&path).ok().as_deref() == Some(text.as_slice()) {
        return Ok(false);
    }
    atomic_write(&path, &text)?;
    Ok(true)
}
pub fn refresh_excludes(repo: &Repo) -> Result<()> {
    excludes(repo)?;
    Ok(())
}

fn walk(
    repo: &Repo,
    relative: &str,
    files: &mut BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
) -> Result<()> {
    let absolute = if relative.is_empty() {
        repo.root.clone()
    } else {
        repo.safe_path(relative)?
    };
    let meta = fs::symlink_metadata(&absolute)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        files.insert(relative.to_owned());
        return Ok(());
    }
    if !relative.is_empty() && absolute.join(".git").exists() {
        return Ok(());
    }
    for entry in fs::read_dir(absolute)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF-8 filenames are unsupported"))?;
        if name.eq_ignore_ascii_case(".git")
            || (relative.is_empty() && name.eq_ignore_ascii_case(".overgit"))
        {
            continue;
        }
        let path = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        if gitlinks.contains(path.as_str())
            || (entry.file_type()?.is_dir() && entry.path().join(".git").exists())
        {
            continue;
        }
        walk(repo, &path, files, gitlinks)?;
    }
    Ok(())
}
pub fn expand(repo: &Repo, inputs: &[PathBuf], add: bool, owned_only: bool) -> Result<Vec<String>> {
    ensure!(!inputs.is_empty(), "at least one path is required");
    let manifest = repo.manifest()?;
    let index = if add || owned_only {
        std::collections::BTreeMap::new()
    } else {
        repo.index(false)?
    };
    let mut paths = BTreeSet::new();
    let gitlinks = repo.gitlinks()?;
    for input in inputs {
        let path = repo.relative_path(input)?;
        if !path.is_empty() {
            repo.safe_path(&path)?;
        }
        if add {
            walk(repo, &path, &mut paths, &gitlinks)?;
        } else {
            let prefix = format!("{path}/");
            let mut found = false;
            for candidate in manifest
                .entries
                .keys()
                .chain(index.keys().filter(|_| !owned_only))
            {
                if path.is_empty() || candidate == &path || candidate.starts_with(&prefix) {
                    paths.insert(candidate.clone());
                    found = true;
                }
            }
            ensure!(
                found,
                "no matching {} paths at {path:?}",
                if owned_only {
                    "owned"
                } else {
                    "tracked or owned"
                }
            );
        }
    }
    Ok(paths.into_iter().collect())
}

pub fn add(repo: &Repo, inputs: &[PathBuf], force: bool) -> Result<i32> {
    repo.assert_attached()?;
    repo.assert_no_sync()?;
    let paths = expand(repo, inputs, true, false)?;
    let mut manifest = repo.manifest()?;
    let indexed = repo.index(true)?;
    let mut content = Vec::new();
    for path in paths {
        let identity = crate::model::path_identity(&path);
        ensure!(
            indexed
                .keys()
                .all(|saved| saved == &path || crate::model::path_identity(saved) != identity),
            "ambiguous path {path:?}; use the existing overlay index spelling"
        );
        repo.staged_base_safe(&path)?;
        if manifest.entries.contains_key(&path) && !force {
            crate::say!("already owned {path:?}; use --force to restage");
            continue;
        }
        let value = repo
            .work(&path)?
            .with_context(|| format!("missing file {path:?}"))?;
        let base = repo.indexed(&path, false)?;
        if base.is_none() {
            ignore_pattern(&path)?;
        }
        let previous = manifest.entries.get(&path);
        let fork = previous.and_then(|entry| entry.base.clone()).or(base);
        let kind = if fork.is_some() {
            Kind::Override
        } else {
            Kind::Add
        };
        manifest
            .entries
            .insert(path.clone(), Entry { kind, base: fork });
        content.push((path, value));
    }
    manifest.validate()?;
    let protection = exclude_bytes(repo, Some(&manifest), true)?;
    let changed_paths: Vec<_> = content.iter().map(|(path, _)| path.clone()).collect();
    crate::mutation::run(repo, &changed_paths, || {
        atomic_write(&repo.common.join("info/exclude"), &protection)?;
        repo.skip_many(content.iter().map(|(path, _)| path.as_str()), true)?;
        for (path, value) in &content {
            rescue_index_before_replacing(repo, path, Some(value))?;
            repo.stage(path, value)?;
        }
        repo.save_manifest(&manifest)
    })?;
    for path in changed_paths {
        crate::say!("owned {path:?}");
    }

    Ok(0)
}
fn expected(repo: &Repo, path: &str, kind: Kind) -> Result<bool> {
    let work = repo.work(path)?;
    if kind == Kind::Delete {
        return Ok(work.is_none());
    }
    Ok(work.is_none() || work == repo.indexed(path, true)?)
}
#[derive(Clone, Copy)]
pub enum Removal {
    Whiteout,
    Restore { all: bool, keep_file: bool },
}

pub fn remove(repo: &Repo, inputs: &[PathBuf], force: bool, action: Removal) -> Result<i32> {
    let (restore, all, keep_file) = match action {
        Removal::Whiteout => (false, false, false),
        Removal::Restore { all, keep_file } => (true, all, keep_file),
    };
    repo.assert_attached()?;
    repo.assert_no_sync()?;
    let mut manifest = repo.manifest()?;
    let paths = if all {
        manifest.entries.keys().cloned().collect()
    } else {
        expand(repo, inputs, false, restore)?
    };
    let mut plans = Vec::new();
    for path in paths {
        repo.safe_path(&path)?;
        let prior = manifest.entries.get(&path).cloned();
        let base = repo.indexed(&path, false)?;
        let clean = if let Some(entry) = &prior {
            expected(repo, &path, entry.kind)?
        } else {
            repo.work(&path)? == base
        };
        ensure!(
            clean || force,
            "uncommitted changes at {path:?}; use --force to back them up first"
        );
        if !restore {
            repo.staged_base_safe(&path)?;
        }
        let value = if restore { base.clone() } else { None };
        let keep =
            restore && keep_file && prior.as_ref().is_some_and(|entry| entry.kind == Kind::Add);
        let entry = if !restore && base.is_some() {
            Some(Entry {
                kind: Kind::Delete,
                base: prior.and_then(|entry| entry.base).or(base),
            })
        } else {
            None
        };
        if let Some(entry) = entry {
            manifest.entries.insert(path.clone(), entry);
        } else {
            manifest.entries.remove(&path);
        }
        let work = repo.work(&path)?;
        let backup = !clean || (work.is_some() && work != value);
        plans.push((path, value, backup, keep));
    }
    manifest.validate()?;
    let protection = exclude_bytes(repo, Some(&manifest), true)?;
    let paths: Vec<_> = plans.iter().map(|(path, ..)| path.clone()).collect();
    crate::mutation::run(repo, &paths, || {
        let (hide, unhide): (Vec<_>, Vec<_>) = plans
            .iter()
            .map(|(path, ..)| path.as_str())
            .partition(|path| manifest.entries.contains_key(*path));
        repo.skip_many(hide, true)?;
        for (path, value, backup, keep) in &plans {
            rescue_index_before_replacing(repo, path, None)?;
            if !keep {
                repo.replace(path, value.as_ref(), *backup, force)?;
            }
            repo.unstage_path(path)?;
        }
        repo.skip_many(unhide, false)?;
        repo.save_manifest(&manifest)?;
        atomic_write(&repo.common.join("info/exclude"), &protection)
    })?;
    for (path, ..) in plans {
        crate::say!("{} {path:?}", if restore { "restored" } else { "removed" });
    }
    Ok(0)
}

pub fn validate_overlay(repo: &Repo, manifest: &Manifest) -> Result<()> {
    for (path, (mode, _)) in repo.index(true)? {
        if path == MANIFEST {
            ensure!(mode == "100644", "manifest must be a regular file");
            continue;
        }
        crate::model::validate_path(&path)?;
        let entry = manifest
            .entries
            .get(&path)
            .with_context(|| format!("overlay index contains unowned file {path:?}; run doctor"))?;
        ensure!(
            entry.kind != Kind::Delete,
            "whiteout unexpectedly tracked by overlay at {path:?}; run doctor"
        );
    }
    for (path, entry) in &manifest.entries {
        repo.safe_path(path)?;
        if entry.kind != Kind::Delete {
            ensure!(
                repo.indexed(path, true)?.is_some(),
                "missing overlay content at {path:?}; recover the index from Git history"
            );
        }
    }
    Ok(())
}

pub fn reconcile(repo: &Repo, dry_run: bool, force: bool) -> Result<bool> {
    repo.assert_idle()?;
    let manifest = repo.manifest()?;
    validate_overlay(repo, &manifest)?;
    let flags = repo.base(["ls-files", "-v", "-z"])?;
    let hidden: BTreeSet<_> = flags
        .split(|byte| *byte == 0)
        .filter(|row| row.first() == Some(&b'S'))
        .filter_map(|row| std::str::from_utf8(row.get(2..)?).ok().map(str::to_owned))
        .collect();
    let index = repo.index(false)?;
    let mut plans = Vec::new();
    for (path, entry) in &manifest.entries {
        let wanted = if entry.kind == Kind::Delete {
            None
        } else {
            repo.indexed(path, true)?
        };
        let work = repo.work(path);
        let blocked = work.is_err();
        let work = if blocked { None } else { work? };
        if blocked || work != wanted {
            if blocked {
                ensure!(
                    force || dry_run,
                    "blocked path {path:?}; use --force to rescue it"
                );
            }
            let unexpected = work.is_some() && work != repo.indexed(path, false)?;
            plans.push((path, wanted, unexpected));
        }
    }
    let text = exclude_bytes(repo, Some(&manifest), true)?;
    let exclude_changed =
        fs::read(repo.common.join("info/exclude")).ok().as_deref() != Some(text.as_slice());
    let flags_changed = manifest
        .entries
        .keys()
        .any(|path| index.contains_key(path) && !hidden.contains(path));
    let changed = !plans.is_empty() || exclude_changed || flags_changed || repo.detached();
    if !dry_run {
        atomic_write(&repo.common.join("info/exclude"), &text)?;
        repo.skip_many(manifest.entries.keys().map(String::as_str), true)?;
    }
    for (path, value, backup) in &plans {
        if dry_run {
            crate::say!("would apply {path:?}");
        } else {
            repo.replace(path, value.as_ref(), *backup, force)?;
            crate::say!("applied {path:?}");
        }
    }
    if !dry_run && repo.detached() {
        fs::remove_file(repo.local("detached.json"))?;
    }
    Ok(changed)
}

pub fn stage_owned(repo: &Repo) -> Result<()> {
    repo.assert_attached()?;
    repo.assert_no_sync()?;
    let manifest = repo.manifest()?;
    validate_overlay(repo, &manifest)?;
    for (path, entry) in &manifest.entries {
        if entry.kind == Kind::Delete {
            continue;
        }
        if let Some(value) = repo.work(path)? {
            rescue_index_before_replacing(repo, path, Some(&value))?;
            repo.stage(path, &value)?;
        } else {
            eprintln!("missing owned file {path:?}; kept staged content, run `overgit apply`");
        }
    }
    repo.save_manifest(&manifest)
}
pub fn rescue_index_before_replacing(
    repo: &Repo,
    path: &str,
    replacement: Option<&crate::model::Snapshot>,
) -> Result<()> {
    if let Some(saved) = repo.indexed(path, true)?
        && replacement != Some(&saved)
        && repo.committed(path)?.as_ref() != Some(&saved)
    {
        repo.rescue(path, &saved)?;
    }
    Ok(())
}

pub fn detach(repo: &Repo, force: bool) -> Result<i32> {
    repo.assert_no_sync()?;
    if repo.detached() {
        crate::say!("already detached; use attach to recover an interrupted detach");
        return Ok(0);
    }
    let manifest = repo.manifest()?;
    validate_overlay(repo, &manifest)?;
    let mut plans = Vec::new();
    for (path, entry) in &manifest.entries {
        let work = repo.work(path)?;
        let base = repo.indexed(path, false)?;
        ensure!(
            entry.kind != Kind::Delete || work.is_none() || work == base || force,
            "whiteout {path:?} has unexpected content; use --force to rescue it"
        );
        let saved = repo.indexed(path, true)?;
        let known_base = work.is_some() && (work == base || work == entry.base);
        let stage = entry.kind != Kind::Delete && work.is_some() && !(known_base && work != saved);
        if !stage && work.is_some() && entry.kind != Kind::Delete && work != saved {
            crate::say!("preserved saved overlay for {path:?}; working bytes match the base");
        }
        plans.push((path.clone(), base, work, entry.kind, stage));
    }
    let paths: Vec<_> = plans.iter().map(|(path, ..)| path.clone()).collect();
    let head = git::text(repo.overlay(["rev-parse", "HEAD"])?)?;
    let mut marker = serde_json::json!({"version":1,"complete":false,"started_unix_secs":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),"overlay_head":head,"paths":paths,"restored":[]});
    let protection = exclude_bytes(repo, Some(&manifest), false)?;
    crate::mutation::run(repo, &paths, || {
        atomic_write(&repo.local("detached.json"), &serde_json::to_vec(&marker)?)?;
        for (path, base, work, kind, stage) in &plans {
            if *stage && let Some(value) = work {
                rescue_index_before_replacing(repo, path, Some(value))?;
                repo.stage(path, value)?;
            }
            if work != base {
                repo.replace(
                    path,
                    base.as_ref(),
                    *kind == Kind::Delete && work.is_some(),
                    force,
                )?;
            }
        }
        repo.skip_many(paths.iter().map(String::as_str), false)?;
        atomic_write(&repo.common.join("info/exclude"), &protection)?;
        marker["restored"] = serde_json::to_value(&paths)?;
        marker["complete"] = true.into();
        atomic_write(&repo.local("detached.json"), &serde_json::to_vec(&marker)?)
    })?;
    crate::say!(
        "detached {} paths; run base Git commands, then `overgit attach`",
        plans.len()
    );
    Ok(0)
}
