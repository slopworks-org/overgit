use crate::{
    git,
    model::{MANIFEST, Snapshot, parents, validate_path},
    repo::{Repo, atomic_write, ensure_not_symlink, leaf_metadata},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Before {
    version: u32,
    head: Option<String>,
    index: Option<Snapshot>,
    shared_indexes: BTreeMap<String, Snapshot>,
    indexed: BTreeMap<String, Snapshot>,
    manifest: Option<Snapshot>,
    exclude: Option<Snapshot>,
    detached: Option<Snapshot>,
    work: BTreeMap<String, Option<Snapshot>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    directories: BTreeMap<String, bool>,
    flags: BTreeMap<String, Option<u8>>,
}

pub fn pending(repo: &Repo) -> bool {
    repo.local("mutation.json").exists()
}
pub fn assert_clear(repo: &Repo) -> Result<()> {
    ensure!(
        !pending(repo),
        "an interrupted local mutation needs recovery; run `overgit doctor --fix` before changing the overlay"
    );
    Ok(())
}
fn raw(path: &Path) -> Result<Option<Snapshot>> {
    ensure_not_symlink(path)?;
    match fs::read(path) {
        Ok(data) => Ok(Some(Snapshot {
            mode: "100644".into(),
            data,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn index_path(repo: &Repo) -> Result<PathBuf> {
    Ok(
        git::text(repo.overlay(["rev-parse", "--path-format=absolute", "--git-path", "index"])?)?
            .into(),
    )
}
fn head(repo: &Repo) -> Result<Option<String>> {
    let output = git::output(
        repo.overlay_cmd()
            .args(["rev-parse", "--verify", "--quiet", "HEAD"]),
        None,
    )?;
    if output.status.success() {
        return Ok(Some(git::text(output.stdout)?));
    }
    ensure!(output.status.code() == Some(1), "cannot read overlay HEAD");
    Ok(None)
}
fn capture(repo: &Repo, paths: &[String]) -> Result<Before> {
    let repo = &repo.with_layout(paths);
    let index = index_path(repo)?;
    ensure!(
        !index.with_extension("lock").exists(),
        "overlay Git index is locked"
    );
    let mut shared_indexes = BTreeMap::new();
    let directory = index.parent().context("index has no parent")?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with("sharedindex.")
            && let Some(value) = raw(&entry.path())?
        {
            shared_indexes.insert(name, value);
        }
    }
    let mut indexed = BTreeMap::new();
    for path in repo.index(true)?.keys() {
        if let Some(value) = repo.indexed(path, true)? {
            indexed.insert(path.clone(), value);
        }
    }
    let mut work = BTreeMap::new();
    let mut flags = BTreeMap::new();
    let base_flags_raw = repo.base(["ls-files", "-v", "-z"])?;
    let base_flags: BTreeMap<&[u8], u8> = base_flags_raw
        .split(|byte| *byte == 0)
        .filter_map(|row| Some((row.get(2..)?, *row.first()?)))
        .collect();
    for path in paths {
        work.insert(path.clone(), repo.work(path)?);
        flags.insert(path.clone(), base_flags.get(path.as_bytes()).copied());
    }
    Ok(Before {
        version: 1,
        head: head(repo)?,
        index: raw(&index)?,
        shared_indexes,
        indexed,
        manifest: raw(&repo.root.join(MANIFEST))?,
        exclude: raw(&repo.common.join("info/exclude"))?,
        detached: raw(&repo.local("detached.json"))?,
        directories: directory_state(repo, work.keys())?,
        work,
        flags,
    })
}
pub fn run<T>(repo: &Repo, paths: &[String], change: impl FnOnce() -> Result<T>) -> Result<T> {
    assert_clear(repo)?;
    let before = capture(repo, paths)?;
    atomic_write(&repo.local("mutation.json"), &serde_json::to_vec(&before)?)?;
    match change() {
        Ok(value) => { fs::remove_file(repo.local("mutation.json"))?; Ok(value) }
        Err(error) => match recover(repo) {
            Ok(()) => Err(error.context("local mutation failed and was rolled back")),
            Err(recovery) => Err(error.context(format!("recovery is incomplete ({recovery:#}); original bytes remain in .overgit/local/mutation.json; run doctor --fix after removing the obstruction"))),
        },
    }
}
pub fn preflight(repo: &Repo, paths: &[String]) -> Result<()> {
    assert_clear(repo)?;
    capture(repo, paths)?;
    Ok(())
}
fn restore_raw(repo: &Repo, path: &Path, label: &str, wanted: Option<&Snapshot>) -> Result<()> {
    let current = raw(path)?;
    if current.as_ref() == wanted {
        return Ok(());
    }
    if let Some(current) = &current {
        repo.rescue(label, current)?;
    }
    if let Some(wanted) = wanted {
        atomic_write(path, &wanted.data)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}
pub fn recover(repo: &Repo) -> Result<()> {
    let state: Before = serde_json::from_slice(&fs::read(repo.local("mutation.json"))?)?;
    let original_files = state
        .manifest
        .as_ref()
        .and_then(|s| serde_json::from_slice::<crate::model::Manifest>(&s.data).ok())
        .filter(|m| m.has_masks() && m.validate().is_ok())
        .map(|m| repo.managed_files(&m))
        .transpose()?
        .unwrap_or_default();
    let repo = &repo.with_layout(state.work.keys().chain(original_files.keys()));
    ensure!(state.version == 1, "unsupported mutation recovery version");
    ensure!(
        head(repo)? == state.head,
        "overlay HEAD changed after the interrupted operation; preserve mutation.json and restore it manually"
    );
    for (path, value) in &state.work {
        repo.validate_leaf(path)?;
        if let Some(value) = value {
            value.validate()?;
        }
    }
    let index = index_path(repo)?;
    let lock = index.with_file_name("index.lock");
    ensure_not_symlink(&lock)?;
    let _index_guard = GitIndexLock::acquire(lock)?;
    for (name, value) in &state.shared_indexes {
        ensure!(
            name.starts_with("sharedindex.") && !name.contains('/'),
            "invalid shared index name"
        );
        let file = index.parent().context("index has no parent")?.join(name);
        if !file.try_exists()? {
            atomic_write(&file, &value.data)?;
        }
    }
    // A raw index is not a reachability root after it is moved out of Git metadata.
    // Rescue the payload of every newly staged version before restoring the old index.
    // A concurrent unresolved Git merge is rejected by the strict overlay index parser.
    for path in repo.index(true)?.keys() {
        if let Some(current) = repo.indexed(path, true)?
            && state.indexed.get(path) != Some(&current)
        {
            repo.rescue(path, &current)?;
        }
    }
    // Recreate saved objects as well as the raw index. Dangling blobs may have been
    // pruned while recovery was pending, and a split index has an extra dependency.
    for value in state.indexed.values() {
        let output = git::output(
            repo.overlay_cmd().args(["hash-object", "-w", "--stdin"]),
            Some(&value.data),
        )?;
        git::check(&output)?;
    }
    recover_work(repo, &state.work, &state.directories)?;
    restore_raw(
        repo,
        &index,
        "overlay index before rollback",
        state.index.as_ref(),
    )?;
    restore_raw(
        repo,
        &repo.root.join(MANIFEST),
        MANIFEST,
        state.manifest.as_ref(),
    )?;
    restore_raw(
        repo,
        &repo.local("detached.json"),
        "detach marker before rollback",
        state.detached.as_ref(),
    )?;
    // The exclude file is shared by sibling worktrees. Preserve its current unmanaged
    // bytes and regenerate the union, rather than restoring a stale whole-file copy.
    let protection = recovery_protection(repo, &state)?;
    let exclude = crate::ops::exclude_bytes(repo, Some(&protection), !repo.detached())?;
    atomic_write(&repo.common.join("info/exclude"), &exclude)?;
    let current_raw = repo.base(["ls-files", "-v", "-z"])?;
    let current_paths: std::collections::BTreeSet<&[u8]> = current_raw
        .split(|byte| *byte == 0)
        .filter_map(|row| row.get(2..))
        .collect();
    let mut flag_updates = Vec::new();
    for (path, flag) in &state.flags {
        if !current_paths.contains(path.as_bytes()) {
            continue;
        }
        let Some(flag) = flag else {
            bail!(
                "base started tracking {path:?} while recovery was pending; resolve it explicitly"
            );
        };
        flag_updates.push((
            path.as_str(),
            flag.eq_ignore_ascii_case(&b'S'),
            flag.is_ascii_lowercase(),
        ));
    }
    repo.set_base_flags(flag_updates)?;
    fs::remove_file(repo.local("mutation.json"))?;
    Ok(())
}
pub(crate) fn directory_state<'a>(
    repo: &Repo,
    paths: impl IntoIterator<Item = &'a String>,
) -> Result<BTreeMap<String, bool>> {
    let mut directories = BTreeMap::new();
    for path in paths {
        for dir in parents(path) {
            if !directories.contains_key(dir) {
                directories.insert(
                    dir.to_owned(),
                    leaf_metadata(&repo.root, dir)?.is_some_and(|m| m.is_dir()),
                );
            }
        }
    }
    Ok(directories)
}

pub(crate) fn recover_work(
    repo: &Repo,
    work: &BTreeMap<String, Option<Snapshot>>,
    directories: &BTreeMap<String, bool>,
) -> Result<()> {
    for dir in directories.keys() {
        validate_path(dir)?;
        let prefix = format!("{dir}/");
        ensure!(
            work.range(prefix.clone()..)
                .next()
                .is_some_and(|(p, _)| p.starts_with(&prefix)),
            "unrelated directory in recovery snapshot {dir:?}"
        );
    }
    let mut leaves: Vec<_> = work.iter().collect();
    leaves.sort_by_key(|(_, value)| value.is_some());
    for (path, value) in leaves {
        let current = match repo.work(path) {
            // Older recovery restores could journal an absent path whose invalid
            // filename was masked by a missing parent. There are no original bytes
            // to restore there. Keep any created directories rather than deleting
            // unjournaled content, and finish recovering the remaining state.
            Err(error)
                if value.is_none()
                    && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        error.kind() == std::io::ErrorKind::InvalidFilename
                    }) =>
            {
                continue;
            }
            other => other?,
        };
        if current != *value {
            repo.replace(path, value.as_ref(), current.is_some(), false)?;
        }
    }
    // Recreate empty directories that a mask pruned. Remove newly created parents
    // bottom-up, leaving later untracked files and directories with contents alone.
    for (dir, existed) in directories {
        if *existed && leaf_metadata(&repo.root, dir)?.is_none() {
            fs::create_dir(repo.safe_path(dir)?)?;
        }
    }
    for (dir, existed) in directories.iter().rev() {
        if !*existed && leaf_metadata(&repo.root, dir)?.is_some_and(|m| m.is_dir()) {
            match fs::remove_dir(repo.safe_path(dir)?) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(crate) struct GitIndexLock {
    path: PathBuf,
    _file: fs::File,
}
impl GitIndexLock {
    pub(crate) fn acquire(path: PathBuf) -> Result<Self> {
        let file = fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("cannot lock overlay index for recovery")?;
        Ok(Self { path, _file: file })
    }
}
impl Drop for GitIndexLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            eprintln!(
                "cannot remove recovery index lock {} ({error})",
                self.path.display()
            );
        }
    }
}

fn recovery_protection(repo: &Repo, state: &Before) -> Result<crate::model::Manifest> {
    let manifest = if let Ok(manifest) = repo.manifest() {
        manifest
    } else {
        let mut manifest = crate::model::Manifest::default();
        for path in state
            .indexed
            .keys()
            .filter(|path| path.as_str() != MANIFEST)
        {
            if crate::model::validate_path(path).is_ok() {
                let base = repo.indexed(path, false)?;
                manifest.entries.insert(
                    path.clone(),
                    crate::model::Entry {
                        kind: if base.is_some() {
                            crate::model::Kind::Override
                        } else {
                            crate::model::Kind::Add
                        },
                        base,
                    },
                );
            }
        }
        manifest.validate()?;
        manifest
    };
    Ok(manifest)
}
