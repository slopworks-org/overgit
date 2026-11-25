use crate::{
    git,
    model::{Entry, Kind, MANIFEST, Manifest, Snapshot},
    mutation, ops,
    repo::{Repo, atomic_write},
};
use anyhow::{Context as _, Result, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write as _},
    path::PathBuf,
};

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
}
#[derive(Serialize)]
pub struct Problem {
    pub severity: Severity,
    pub code: &'static str,
    pub path: Option<String>,
    pub fixable: bool,
    pub message: String,
    pub hint: &'static str,
}
fn problem(
    code: &'static str,
    path: Option<&str>,
    fixable: bool,
    message: impl Into<String>,
    hint: &'static str,
) -> Problem {
    Problem {
        severity: Severity::Warning,
        code,
        path: path.map(str::to_owned),
        fixable,
        message: message.into(),
        hint,
    }
}

pub fn run(repo: &Repo, fix: bool, rebuild: bool, porcelain: bool) -> Result<i32> {
    if fix && repo.require_overlay().is_ok() && repo.assert_idle().is_ok() {
        if mutation::pending(repo) {
            mutation::recover(repo).context("cannot recover interrupted local mutation; original snapshots remain in mutation.json")?;
        }
        if !repo.local("sync.json").exists() && crate::pull::state_summary(repo)?.is_none() {
            let mut paths: BTreeSet<_> = repo
                .index(true)?
                .keys()
                .filter(|path| path.as_str() != MANIFEST)
                .cloned()
                .collect();
            if let Some(manifest) = load_manifest(repo, rebuild, &mut Vec::new())? {
                paths.extend(repo.managed_files(&manifest)?.into_keys());
            }
            let repo = &repo.with_layout(&paths);
            let paths: Vec<_> = paths
                .into_iter()
                .filter(|path| crate::model::validate_path(path).is_ok() && repo.work(path).is_ok())
                .collect();
            mutation::run(repo, &paths, || {
                examine(repo, true, rebuild)?;
                Ok(())
            })?;
        }
    }
    let problems = diagnose(repo)?;
    let mut out = io::stdout().lock();
    if porcelain {
        serde_json::to_writer_pretty(&mut out, &problems)?;
        writeln!(out)?;
    } else if problems.is_empty() {
        writeln!(out, "healthy")?;
    } else {
        for finding in &problems {
            writeln!(
                out,
                "{}{}  {}",
                finding.code,
                finding
                    .path
                    .as_ref()
                    .map_or(String::new(), |path| format!(" {path:?}")),
                finding.message
            )?;
            writeln!(out, "  {}", finding.hint)?;
        }
    }
    Ok(
        if problems
            .iter()
            .any(|problem| problem.severity == Severity::Warning)
        {
            4
        } else {
            0
        },
    )
}
pub fn diagnose(repo: &Repo) -> Result<Vec<Problem>> {
    examine(&repo.for_inspection(), false, false)
}
fn examine(repo: &Repo, fix: bool, rebuild: bool) -> Result<Vec<Problem>> {
    let mut problems = Vec::new();
    context(repo, &mut problems)?;
    if let Err(error) = repo.require_overlay() {
        problems.push(problem("overlay-unavailable", None, false, format!("overlay storage is absent or damaged ({error:#})"), "restore .overgit/.git from a safe backup or clone; existing project bytes are retained"));
        if repo.root.join(".overgit").exists() {
            problems.push(problem(
                "clean-unprotected",
                None,
                false,
                "overlay metadata may be removed by base git clean",
                "avoid git clean until the nested overlay repository is restored",
            ));
        }
        return Ok(problems);
    }
    let index = match repo.index(true) {
        Ok(index) => index,
        Err(error) => {
            problems.push(problem(
                "overlay-index-invalid",
                None,
                false,
                error.to_string(),
                "finish the overlay Git operation or restore its index from a safe commit",
            ));
            return Ok(problems);
        }
    };
    let Some(mut manifest) = load_manifest(repo, rebuild, &mut problems)? else {
        return Ok(problems);
    };
    if !index.contains_key(MANIFEST) {
        problems.push(problem(
            "manifest-untracked",
            Some(MANIFEST),
            true,
            "manifest is missing from the overlay index",
            "run doctor --fix",
        ));
    }
    let orphans: Vec<_> = index
        .keys()
        .filter(|path| path.as_str() != MANIFEST && !manifest.entries.contains_key(*path))
        .cloned()
        .collect();
    for path in &orphans {
        problems.push(problem("orphan-overlay-file", Some(path), true, "overlay index has undeclared content", "run doctor --fix --rebuild to infer ownership and reset unknown fork points against the current base"));
        if fix && rebuild {
            manifest
                .entries
                .insert(path.clone(), inferred_entry(repo, path)?);
        }
    }
    manifest.validate()?;
    if fix {
        protect_and_save_manifest(repo, &manifest, rebuild && !orphans.is_empty())?;
    }
    let files = repo.managed_files(&manifest)?;
    let repo = &repo.with_layout(files.keys());
    base_context(repo, &manifest, &mut problems)?;
    detach_context(repo, &manifest, &files, &mut problems)?;
    check_config(repo, fix, &mut problems)?;
    let staged = opaque_context(repo, &manifest, &mut problems)?;
    check_flags(repo, &files, &staged, fix, &mut problems)?;
    let mut paths: Vec<_> = files.keys().collect();
    paths.sort_by_key(|path| manifest.entries.contains_key(*path));
    for path in paths {
        let result = if let Some(entry) = manifest.entries.get(path) {
            check_file(repo, path, entry, fix, &mut problems)
        } else if !repo.detached() && !index.contains_key(path) && !staged.contains(path) {
            check_opaque_file(repo, &manifest, path, fix, &mut problems)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            problems.push(problem(
                "path-unavailable",
                Some(path),
                false,
                error.to_string(),
                "repair the reported path or Git index obstruction before applying content",
            ));
        }
    }
    check_excludes(repo, &manifest, fix, &mut problems)?;
    Ok(problems)
}

fn opaque_context(
    repo: &Repo,
    manifest: &Manifest,
    problems: &mut Vec<Problem>,
) -> Result<BTreeSet<String>> {
    let staged = repo.staged_opaque_files(manifest)?;
    for path in &staged {
        problems.push(problem(
            "base-index-divergence",
            Some(path),
            false,
            "base has staged changes inside an opaque directory",
            "commit or unstage the base changes before repairing the directory mask",
        ));
    }
    for dir in &manifest.opaque {
        problems.push(Problem {
            severity: Severity::Info,
            code: "opaque-directory",
            path: Some(dir.clone()),
            fixable: false,
            message: "base files in this directory are hidden while attached".into(),
            hint: "use restore on the directory to expose its base files",
        });
    }
    Ok(staged)
}

fn check_excludes(
    repo: &Repo,
    manifest: &Manifest,
    fix: bool,
    problems: &mut Vec<Problem>,
) -> Result<()> {
    match ops::exclude_bytes(repo, Some(manifest), !repo.detached()) {
        Ok(wanted) => {
            if fs::read(repo.common.join("info/exclude")).ok().as_deref() != Some(wanted.as_slice())
            {
                problems.push(problem(
                    "base-exclude-drift",
                    None,
                    true,
                    "managed base exclude block needs repair",
                    "run doctor --fix",
                ));
                if fix {
                    atomic_write(&repo.common.join("info/exclude"), &wanted)?;
                }
            }
        }
        Err(error) => problems.push(problem(
            "base-exclude-unavailable",
            None,
            false,
            error.to_string(),
            "repair the exclude file or sibling worktree before changing ownership",
        )),
    }
    Ok(())
}

fn context(repo: &Repo, problems: &mut Vec<Problem>) -> Result<()> {
    recovery_context(repo, problems);
    if let Err(error) = repo.assert_idle() {
        let message = error.to_string();
        let code = if message.contains("sparse") {
            "base-sparse-checkout"
        } else {
            "base-operation-in-progress"
        };
        problems.push(problem(code, None, false, message, "finish or abort the base operation, or disable sparse checkout, before mutating the overlay"));
    }
    if mutation::pending(repo) {
        problems.push(problem(
            "mutation-interrupted",
            None,
            true,
            "a local mutation has a durable recovery snapshot",
            "run doctor --fix to restore the original state; changed bytes are rescued first",
        ));
    }
    sync_context(repo, problems);
    match crate::pull::state_summary(repo) {
        Ok(Some(state)) => problems.push(problem(
            "pull-in-progress",
            None,
            false,
            state.to_string(),
            "use pull --status, pull --git, pull --continue or pull --abort",
        )),
        Err(error) => problems.push(problem(
            "pull-state-invalid",
            None,
            false,
            error.to_string(),
            "preserve .overgit/local/pull before attempting recovery",
        )),
        _ => {}
    }
    if repo.detached() {
        match fs::read(repo.local("detached.json"))
            .ok()
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
        {
            Some(marker) if marker["complete"] == false => problems.push(problem(
                "detach-interrupted",
                None,
                false,
                "detach is incomplete",
                "run attach to restore saved overlay content; doctor does not silently remount it",
            )),
            None => problems.push(problem(
                "detach-marker-invalid",
                None,
                false,
                "detach marker is unreadable",
                "inspect saved overlay content and run attach when ready",
            )),
            _ => {}
        }
    }
    let ignore = repo.root.join(".gitignore");
    if repo.index_file(".gitignore", false)?.is_some()
        && let Ok(bytes) = fs::read(ignore)
        && bytes.split(|byte| *byte == b'\n').any(|line| {
            matches!(
                line.strip_suffix(b"\r").unwrap_or(line),
                b"# overgit begin" | b"# overgit end"
            )
        })
    {
        problems.push(problem("tracked-ignore-overgit", Some(".gitignore"), false, "tracked ignore file contains an overgit managed marker", "keep local overlay patterns in Git info/exclude; review intentional project rules manually"));
    }
    for finding in problems
        .iter_mut()
        .filter(|finding| matches!(finding.code, "sync-in-progress" | "pull-in-progress"))
    {
        finding.severity = Severity::Info;
    }
    Ok(())
}

fn sync_context(repo: &Repo, problems: &mut Vec<Problem>) {
    let interrupted = crate::sync::is_interrupted(repo);
    match crate::sync::pending_paths(repo) {
        Ok(paths) if repo.local("sync.json").exists() && !interrupted => {
            problems.push(problem(
                "sync-in-progress",
                None,
                false,
                format!("sync has {} pending paths", paths.len()),
                "inspect with sync --dry-run or sync --inspect PATH; resolve and continue, or sync --abort",
            ));
        }
        Err(error) => problems.push(problem(
            "sync-state-invalid",
            None,
            false,
            error.to_string(),
            "preserve .overgit/local/sync.json and recover its snapshots before removing it",
        )),
        _ => {}
    }
    if interrupted {
        problems.push(problem(
            "sync-interrupted",
            None,
            false,
            "sync stopped before applying its full plan",
            "run sync --abort before retrying",
        ));
    }
}

fn recovery_entries(repo: &Repo) -> Result<(usize, usize)> {
    let path = repo.local("backups");
    crate::repo::ensure_not_symlink(&path)?;
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    };
    let mut count = 0;
    let mut unsafe_entries = 0;
    for entry in entries {
        // Inspect entry types only. Recovery payloads and archive trees can be
        // arbitrarily large or incomplete, and must never be traversed here.
        let kind = entry?.file_type()?;
        count += 1;
        if !kind.is_dir() && !kind.is_file() {
            unsafe_entries += 1;
        }
    }
    Ok((count, unsafe_entries))
}

fn recovery_context(repo: &Repo, problems: &mut Vec<Problem>) {
    const LOCATION: &str = ".overgit/local/backups";
    const UNSAFE_HINT: &str = "inspect local recovery storage manually without following symlinks; preserve its entries before repairing the obstruction";
    match recovery_entries(repo) {
        Ok((0, _)) => {}
        Ok((count, unsafe_entries)) => {
            let mut finding = problem(
                "recovery-entries",
                Some(LOCATION),
                false,
                format!(
                    "{count} local recovery entries; contents and recoverability have not been validated"
                ),
                "use recovery list and recovery inspect ID; recovery restore ID --dry-run previews a working-file restore; metadata snapshots require --to PATH",
            );
            finding.severity = Severity::Info;
            problems.push(finding);
            if unsafe_entries != 0 {
                problems.push(problem(
                    "recovery-storage-unsafe",
                    Some(LOCATION),
                    false,
                    format!(
                        "{unsafe_entries} symlink or special entries were not followed or opened"
                    ),
                    UNSAFE_HINT,
                ));
            }
        }
        Err(error) => problems.push(problem(
            "recovery-storage-unavailable",
            Some(LOCATION),
            false,
            format!("cannot safely inventory local recovery storage ({error:#})"),
            UNSAFE_HINT,
        )),
    }
}
fn parse_manifest(value: &Snapshot) -> Result<Manifest> {
    ensure!(value.mode == "100644", "manifest must be a regular file");
    let manifest: Manifest = serde_json::from_slice(&value.data)?;
    manifest.validate()?;
    Ok(manifest)
}
fn load_manifest(
    repo: &Repo,
    rebuild: bool,
    problems: &mut Vec<Problem>,
) -> Result<Option<Manifest>> {
    if let Ok(manifest) = repo.manifest() {
        return Ok(Some(manifest));
    }
    problems.push(problem("manifest-invalid", Some(MANIFEST), true, "working manifest is missing or invalid", "run doctor --fix to try valid index and HEAD copies; --rebuild explicitly infers ownership if no copy survives"));
    let mut candidates = Vec::new();
    if let Some(indexed) = repo.indexed(MANIFEST, true)? {
        candidates.push(indexed);
    }
    if let Some(committed) = repo.committed(MANIFEST)? {
        candidates.push(committed);
    }
    for value in &candidates {
        if let Ok(manifest) = parse_manifest(value) {
            return Ok(Some(manifest));
        }
    }
    if !rebuild {
        return Ok(None);
    }
    let mut manifest = Manifest::default();
    for path in repo
        .index(true)?
        .keys()
        .filter(|path| path.as_str() != MANIFEST)
    {
        manifest
            .entries
            .insert(path.clone(), inferred_entry(repo, path)?);
    }
    manifest.validate()?;
    Ok(Some(manifest))
}
fn rescue_manifest(repo: &Repo, candidates: &[Snapshot]) -> Result<()> {
    if let Ok(data) = fs::read(repo.root.join(MANIFEST)) {
        repo.rescue(
            MANIFEST,
            &Snapshot {
                mode: "100644".into(),
                data,
            },
        )?;
    }
    for value in candidates {
        if parse_manifest(value).is_err() {
            repo.rescue("invalid indexed manifest", value)?;
        }
    }
    Ok(())
}
fn inferred_entry(repo: &Repo, path: &str) -> Result<Entry> {
    repo.safe_path(path)?;
    let base = repo.indexed(path, false)?;
    Ok(Entry {
        kind: if base.is_some() {
            Kind::Override
        } else {
            Kind::Add
        },
        base,
    })
}
fn check_config(repo: &Repo, fix: bool, problems: &mut Vec<Problem>) -> Result<()> {
    let output = git::output(
        repo.overlay_cmd()
            .args(["config", "--get", "core.worktree"]),
        None,
    )?;
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "cannot read overlay core.worktree"
    );
    let configured = git::text(output.stdout)?;
    let actual_git = PathBuf::from(git::text(
        repo.overlay(["rev-parse", "--absolute-git-dir"])?,
    )?);
    if actual_git.join(configured).canonicalize().ok().as_ref() != Some(&repo.root) {
        problems.push(problem(
            "overlay-worktree-invalid",
            None,
            true,
            "overlay core.worktree is incorrect",
            "run doctor --fix",
        ));
        if fix {
            crate::bootstrap::configure(repo)?;
        }
    }
    for (key, expected) in [("core.bare", "false"), ("status.showUntrackedFiles", "no")] {
        let output = git::output(repo.overlay_cmd().args(["config", "--get", key]), None)?;
        ensure!(
            output.status.success() || output.status.code() == Some(1),
            "cannot read overlay config"
        );
        if git::text(output.stdout)? != expected {
            problems.push(problem(
                "overlay-config-drift",
                None,
                true,
                format!("{key} should be {expected}"),
                "run doctor --fix",
            ));
            if fix {
                crate::bootstrap::configure(repo)?;
            }
        }
    }
    let common = PathBuf::from(git::text(repo.overlay([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?)?);
    let exclude = common.join("info/exclude");
    crate::repo::ensure_not_symlink(&exclude)?;
    let patterns = match fs::read(&exclude) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    if [b"/.git".as_slice(), b"/.overgit/.git/", b"/.overgit/local/"]
        .iter()
        .any(|pattern| {
            !patterns
                .split(|byte| *byte == b'\n')
                .any(|line| line == *pattern)
        })
    {
        problems.push(problem(
            "overlay-exclude-drift",
            None,
            true,
            "overlay metadata exclude patterns need repair",
            "run doctor --fix",
        ));
        if fix {
            crate::bootstrap::configure(repo)?;
        }
    }
    let head = git::output(
        repo.overlay_cmd()
            .args(["rev-parse", "--verify", "--quiet", "HEAD"]),
        None,
    )?;
    if !head.status.success() {
        ensure!(head.status.code() == Some(1), "cannot read overlay HEAD");
        problems.push(problem(
            "overlay-unborn",
            None,
            false,
            "overlay has no commit",
            "run overgit commit to make the saved content portable",
        ));
    }
    Ok(())
}
fn check_flags(
    repo: &Repo,
    files: &BTreeMap<String, Kind>,
    staged: &BTreeSet<String>,
    fix: bool,
    problems: &mut Vec<Problem>,
) -> Result<()> {
    let flags = repo.base(["ls-files", "-v", "-z"])?;
    let mut need_skip = Vec::new();
    for row in flags.split(|byte| *byte == 0).filter(|row| !row.is_empty()) {
        let Some(path) = row
            .get(2..)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
        else {
            continue;
        };
        let owned = files.contains_key(path);
        if row[0].is_ascii_lowercase() {
            problems.push(problem("assume-unchanged", Some(path), owned, "base has an assume-unchanged flag", "owned flags can be repaired with doctor --fix; unowned flags are left under your control"));
            if fix && owned && !staged.contains(path) && repo.work(path).is_ok() {
                repo.base(["update-index", "--no-assume-unchanged", "--", path])?;
            }
        }
        if row[0].eq_ignore_ascii_case(&b'S') {
            if !owned {
                problems.push(problem(
                    "unowned-skip",
                    Some(path),
                    false,
                    "base has an unowned skip-worktree flag",
                    "clear it explicitly if unwanted; doctor does not adopt it",
                ));
            }
        } else if owned && !repo.detached() && !staged.contains(path) {
            problems.push(problem(
                "owned-skip-missing",
                Some(path),
                true,
                "owned path is not hidden from the base",
                "run doctor --fix",
            ));
            if fix && repo.work(path).is_ok() {
                need_skip.push(path);
            }
        }
    }
    repo.skip_many(need_skip, true)?;
    Ok(())
}
fn check_opaque_file(
    repo: &Repo,
    manifest: &Manifest,
    path: &str,
    fix: bool,
    problems: &mut Vec<Problem>,
) -> Result<()> {
    if let Some(work) = repo.work(path)? {
        problems.push(problem(
            "opaque-file-visible",
            Some(path),
            true,
            "base file is visible inside an opaque directory",
            "run sync or doctor --fix to hide it and back up any local edits",
        ));
        if fix {
            let backup = Some(work) != repo.indexed(path, false)?;
            repo.replace(path, None, backup, false)?;
            ops::prune_opaque_parents(repo, manifest, path)?;
        }
    }
    Ok(())
}
fn check_file(
    repo: &Repo,
    path: &str,
    entry: &Entry,
    fix: bool,
    problems: &mut Vec<Problem>,
) -> Result<()> {
    repo.validate_leaf(path)?;
    let base = repo.indexed(path, false)?;
    let upstream = crate::sync::upstream(repo, path)?;
    if base != upstream {
        problems.push(problem(
            "base-index-divergence",
            Some(path),
            false,
            "base index differs from HEAD at an owned path",
            "review and commit or unstage base changes explicitly; doctor preserves them",
        ));
    }
    let transition = match entry.kind {
        Kind::Add if base.is_some() => {
            Some(("add-collision", "base started tracking an overlay addition"))
        }
        Kind::Override | Kind::Delete if upstream.is_none() => Some((
            "upstream-deleted",
            "base HEAD no longer tracks the owned path",
        )),
        _ => None,
    };
    if let Some((code, message)) = transition {
        problems.push(problem(
            code,
            Some(path),
            false,
            message,
            "run sync and choose an explicit ownership decision",
        ));
    }
    if !repo.masked_leaf(path)?
        && let Some(actual) = different_case(repo, path)?
    {
        problems.push(problem(
            "path-case-mismatch",
            Some(path),
            false,
            format!("disk path is spelled {actual:?}"),
            "review the path spelling before applying or reconstructing ownership",
        ));
        return Ok(());
    }
    if repo.detached() {
        return Ok(());
    }
    check_content(repo, path, entry.kind, base.as_ref(), fix, problems)
}
fn check_content(
    repo: &Repo,
    path: &str,
    kind: Kind,
    base: Option<&Snapshot>,
    fix: bool,
    problems: &mut Vec<Problem>,
) -> Result<()> {
    let mut indexed = repo.indexed(path, true)?;
    if kind != Kind::Delete && indexed.is_none() {
        problems.push(problem(
            "overlay-content-missing",
            Some(path),
            true,
            "saved overlay content is missing from the index",
            "run doctor --fix to restore it from HEAD or the existing worktree",
        ));
        if fix {
            indexed = repo.committed(path)?.or(repo.work(path)?);
            if let Some(value) = &indexed {
                repo.stage(path, value)?;
            }
        }
        if indexed.is_none() {
            return Ok(());
        }
    }
    if kind == Kind::Delete
        && let Some(saved) = &indexed
    {
        problems.push(problem(
            "whiteout-tracked",
            Some(path),
            true,
            "whiteout has unexpected saved file content",
            "run doctor --fix to rescue and remove it from the overlay index",
        ));
        if fix {
            repo.rescue(path, saved)?;
            repo.unstage_path(path)?;
        }
    }
    let work = repo.work(path)?;
    let drift = if kind == Kind::Delete {
        work.is_some()
    } else {
        work.is_none()
            || (work.as_ref() == base && work != indexed)
            || work
                .as_ref()
                .zip(indexed.as_ref())
                .is_some_and(|(work, saved)| work.data == saved.data && work.mode != saved.mode)
    };
    if drift {
        problems.push(problem(
            "worktree-drift",
            Some(path),
            true,
            "worktree lost its saved overlay content or mode",
            "run doctor --fix; unexpected existing bytes are rescued before replacement",
        ));
        if fix {
            repo.replace(
                path,
                if kind == Kind::Delete {
                    None
                } else {
                    indexed.as_ref()
                },
                work.is_some() && work.as_ref() != base,
                false,
            )?;
        }
    }
    Ok(())
}
fn different_case(repo: &Repo, path: &str) -> Result<Option<String>> {
    let mut directory = repo.root.clone();
    let mut spelling = Vec::new();
    for component in path.split('/') {
        let mut found = None;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && name.eq_ignore_ascii_case(component)
            {
                found = Some(name.to_owned());
                if name == component {
                    break;
                }
            }
        }
        let Some(found) = found else {
            return Ok(None);
        };
        directory.push(&found);
        spelling.push(found);
        if directory.is_symlink() {
            break;
        }
        if !directory.is_dir() {
            break;
        }
    }
    let actual = spelling.join("/");
    Ok((actual != path).then_some(actual))
}

fn base_context(repo: &Repo, manifest: &Manifest, problems: &mut Vec<Problem>) -> Result<()> {
    let branch = git::output(
        repo.base_cmd().args(["symbolic-ref", "--quiet", "HEAD"]),
        None,
    )?;
    if branch.status.code() == Some(1) {
        let mut finding = problem(
            "base-detached",
            None,
            false,
            "base HEAD is detached",
            "this is supported; select a branch before relying on upstream tracking",
        );
        finding.severity = Severity::Info;
        problems.push(finding);
        return Ok(());
    }
    git::check(&branch)?;
    let branch = git::text(branch.stdout)?;
    let upstream = git::text(repo.base(["for-each-ref", "--format=%(upstream)", &branch])?)?;
    if upstream.is_empty() {
        return Ok(());
    }
    let head = git::output(
        repo.base_cmd()
            .args(["rev-parse", "--verify", "--quiet", "HEAD"]),
        None,
    )?;
    if head.status.code() == Some(1) {
        return Ok(());
    }
    git::check(&head)?;
    let available = git::output(
        repo.base_cmd()
            .args(["rev-parse", "--verify", "--quiet", &upstream]),
        None,
    )?;
    if available.status.code() == Some(1) {
        return Ok(());
    }
    git::check(&available)?;
    let changes = repo.base(["diff", "--name-only", "-z", &format!("HEAD..{upstream}")])?;
    for path in changes.split(|byte| *byte == 0) {
        if let Ok(path) = std::str::from_utf8(path)
            && manifest.manages_tracked(path)
        {
            problems.push(problem(
                "base-pull-owned",
                Some(path),
                false,
                "fetched upstream changes an owned path",
                "detach before updating the base, then attach and sync",
            ));
        }
    }
    Ok(())
}
fn detach_context(
    repo: &Repo,
    manifest: &Manifest,
    files: &BTreeMap<String, Kind>,
    problems: &mut Vec<Problem>,
) -> Result<()> {
    if !repo.detached() || files.is_empty() {
        return Ok(());
    }
    let mut mounted = true;
    for (path, kind) in files {
        let expected = if *kind == Kind::Delete {
            None
        } else {
            repo.indexed(path, true)?
        };
        if repo.work(path)? != expected {
            mounted = false;
            break;
        }
        let flags = repo.base(["ls-files", "-v", "-z", "--", path])?;
        if !flags.is_empty() && !matches!(flags.first(), Some(b'S' | b's')) {
            mounted = false;
            break;
        }
    }
    if mounted {
        let wanted = ops::exclude_bytes(repo, Some(manifest), true)?;
        if fs::read(repo.common.join("info/exclude")).ok().as_deref() == Some(wanted.as_slice()) {
            problems.push(problem(
                "detach-marker-stale",
                None,
                false,
                "detach marker remains although worktree, flags and excludes are mounted",
                "inspect the marker, then use attach to confirm and clear it",
            ));
        }
    }
    Ok(())
}

fn protect_and_save_manifest(repo: &Repo, manifest: &Manifest, reconstructed: bool) -> Result<()> {
    // Establish the complete shared exclusion union before accepting reconstructed
    // ownership or writing any private bytes into the shared working tree.
    let protection = ops::exclude_bytes(repo, Some(manifest), !repo.detached())?;
    atomic_write(&repo.common.join("info/exclude"), &protection)?;
    let indexed = repo.indexed(MANIFEST, true)?;
    let invalid_index = indexed
        .as_ref()
        .is_some_and(|value| parse_manifest(value).is_err());
    if repo.manifest().is_err() || indexed.is_none() || invalid_index || reconstructed {
        let candidates: Vec<_> = indexed.into_iter().collect();
        rescue_manifest(repo, &candidates)?;
        repo.save_manifest(manifest)?;
    }
    Ok(())
}
