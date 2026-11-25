use crate::{
    git,
    model::{Entry, Kind, MANIFEST, Manifest, Snapshot, validate_path},
    repo::{Repo, atomic_write},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write as _, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Situation {
    Conflict,
    Binary,
    Deleted,
    Collision,
    Whiteout,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    situation: Situation,
    ours: Snapshot,
    upstream: Option<Snapshot>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseFlags {
    skip: bool,
    assume: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedIndex {
    name: String,
    snapshot: Snapshot,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    completed: bool,
    manifest: Snapshot,
    index: Snapshot,
    indexed: BTreeMap<String, Snapshot>,
    shared_index: Option<SharedIndex>,
    work: BTreeMap<String, Option<Snapshot>>,
    base_flags: BTreeMap<String, BaseFlags>,
    pending: BTreeMap<String, Pending>,
}

enum Action {
    None,
    Missing,
    Update(Entry, Option<Snapshot>),
    Pending(Pending, Option<Snapshot>),
    Drop,
}

struct Planned {
    path: String,
    before: Option<Snapshot>,
    action: Action,
}

fn read_state(repo: &Repo) -> Result<Option<State>> {
    let path = repo.local("sync.json");
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state: State = serde_json::from_slice(&data)
        .context("sync state is corrupt; preserve it before attempting recovery")?;
    ensure!(
        state.version == 2,
        "unsupported sync state version; preserve state and recover it before retrying"
    );
    let manifest: Manifest = serde_json::from_slice(&state.manifest.data)?;
    manifest.validate()?;
    for snapshot in state.indexed.values() {
        snapshot.validate()?;
    }
    if let Some(shared) = &state.shared_index {
        validate_shared_name(&shared.name)?;
        shared.snapshot.validate()?;
    }
    for (path, snapshot) in &state.work {
        repo.safe_path(path)?;
        if let Some(snapshot) = snapshot {
            snapshot.validate()?;
        }
    }
    for path in state.base_flags.keys() {
        ensure!(
            state.work.contains_key(path),
            "base flags refer to an unrelated path {path:?}"
        );
    }
    for (path, pending) in &state.pending {
        validate_path(path)?;
        ensure!(
            state.work.contains_key(path),
            "sync state is missing its original file for {path:?}"
        );
        pending.ours.validate()?;
        if let Some(upstream) = &pending.upstream {
            upstream.validate()?;
        }
    }
    Ok(Some(state))
}

fn save_state(repo: &Repo, state: &State) -> Result<()> {
    atomic_write(&repo.local("sync.json"), &serde_json::to_vec_pretty(state)?)
}

fn index_path(repo: &Repo) -> Result<PathBuf> {
    Ok(PathBuf::from(git::text(repo.overlay([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "index",
    ])?)?))
}

fn original_state(repo: &Repo, work: BTreeMap<String, Option<Snapshot>>) -> Result<State> {
    Ok(State {
        version: 2,
        completed: false,
        manifest: Snapshot {
            mode: "100644".into(),
            data: fs::read(repo.root.join(MANIFEST))?,
        },
        index: Snapshot {
            mode: "100644".into(),
            data: fs::read(index_path(repo)?)?,
        },
        indexed: indexed_content(repo)?,
        shared_index: shared_index(repo)?,
        base_flags: base_flags(repo, &work)?,
        work,
        pending: BTreeMap::new(),
    })
}

fn indexed_content(repo: &Repo) -> Result<BTreeMap<String, Snapshot>> {
    let mut indexed = BTreeMap::new();
    for path in repo.index(true)?.keys() {
        let snapshot = repo
            .indexed(path, true)?
            .with_context(|| format!("overlay content disappeared at {path:?}"))?;
        snapshot.validate()?;
        indexed.insert(path.clone(), snapshot);
    }
    Ok(indexed)
}

fn restore_indexed_objects(repo: &Repo, original: &BTreeMap<String, Snapshot>) -> Result<()> {
    // Parse and load every current stage-zero payload before changing any index or
    // worktree bytes. An unresolved Git merge is rejected instead of being flattened.
    for (path, current) in indexed_content(repo)? {
        if original.get(&path) != Some(&current) {
            repo.rescue(&path, &current)?;
        }
    }
    for snapshot in original.values() {
        let output = git::output(
            repo.overlay_cmd().args(["hash-object", "-w", "--stdin"]),
            Some(&snapshot.data),
        )?;
        git::check(&output)?;
    }
    Ok(())
}

fn validate_shared_name(name: &str) -> Result<()> {
    let oid = name
        .strip_prefix("sharedindex.")
        .context("invalid shared index name")?;
    ensure!(
        matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid shared index identifier"
    );
    Ok(())
}

fn shared_index(repo: &Repo) -> Result<Option<SharedIndex>> {
    let path = git::text(repo.overlay(["rev-parse", "--shared-index-path"])?)?;
    if path.is_empty() {
        return Ok(None);
    }
    let path = repo.root.join(path);
    let parent = index_path(repo)?
        .parent()
        .context("index has no parent")?
        .canonicalize()?;
    ensure!(
        path.parent()
            .context("shared index has no parent")?
            .canonicalize()?
            == parent,
        "shared index is outside overlay metadata"
    );
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid shared index path")?
        .to_owned();
    validate_shared_name(&name)?;
    crate::repo::ensure_not_symlink(&path)?;
    Ok(Some(SharedIndex {
        name,
        snapshot: Snapshot {
            mode: "100644".into(),
            data: fs::read(path)?,
        },
    }))
}

fn restore_shared_index(repo: &Repo, shared: Option<&SharedIndex>) -> Result<()> {
    let Some(shared) = shared else {
        return Ok(());
    };
    let path = index_path(repo)?
        .parent()
        .context("index has no parent")?
        .join(&shared.name);
    crate::repo::ensure_not_symlink(&path)?;
    if let Ok(bytes) = fs::read(&path)
        && bytes != shared.snapshot.data
    {
        repo.rescue(
            &shared.name,
            &Snapshot {
                mode: "100644".into(),
                data: bytes,
            },
        )?;
    }
    atomic_write(&path, &shared.snapshot.data)
}

fn base_flags(
    repo: &Repo,
    work: &BTreeMap<String, Option<Snapshot>>,
) -> Result<BTreeMap<String, BaseFlags>> {
    let mut flags = BTreeMap::new();
    let selected: BTreeMap<_, _> = work.keys().map(|path| (path.as_bytes(), path)).collect();
    for row in repo
        .base(["ls-files", "-v", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let bytes = row.get(2..).context("invalid base flag record")?;
        if let Some(path) = selected.get(bytes) {
            flags.insert(
                (*path).clone(),
                BaseFlags {
                    skip: row[0].eq_ignore_ascii_case(&b'S'),
                    assume: row[0].is_ascii_lowercase(),
                },
            );
        }
    }
    Ok(flags)
}

fn restore_flags(repo: &Repo, flags: &BTreeMap<String, BaseFlags>) -> Result<()> {
    repo.set_base_flags(
        flags
            .iter()
            .map(|(path, flag)| (path.as_str(), flag.skip, flag.assume)),
    )
}

pub(crate) fn upstream(repo: &Repo, path: &str) -> Result<Option<Snapshot>> {
    if !repo.base_head_exists()? {
        return repo.indexed(path, false);
    }
    let bytes = repo.base_tree_entry(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let record = bytes
        .split(|byte| *byte == 0)
        .next()
        .context("empty tree record")?;
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .context("invalid tree record")?;
    let mut fields = std::str::from_utf8(&record[..tab])?.split_whitespace();
    let mode = fields.next().context("tree mode missing")?.to_owned();
    ensure!(
        fields.next() == Some("blob"),
        "base path {path:?} is not a file"
    );
    let oid = fields.next().context("tree oid missing")?;
    let data = repo.read_object(path, false, &mode, oid)?;
    let snapshot = Snapshot { mode, data };
    snapshot.validate()?;
    Ok(Some(snapshot))
}

enum Merge {
    Clean(Snapshot),
    Conflict(Snapshot),
    Binary,
}

fn merge(
    repo: &Repo,
    base: &Snapshot,
    ours: &Snapshot,
    theirs: &Snapshot,
    style: &str,
) -> Result<Merge> {
    if ours == theirs || theirs == base {
        return Ok(Merge::Clean(ours.clone()));
    }
    if ours == base {
        return Ok(Merge::Clean(theirs.clone()));
    }
    if ours.mode == "120000"
        || theirs.mode == "120000"
        || base.mode == "120000"
        || [&base.data, &ours.data, &theirs.data]
            .iter()
            .any(|data| data.iter().take(8000).any(|byte| *byte == 0))
    {
        return Ok(Merge::Binary);
    }
    let mode = if ours.mode == base.mode {
        theirs.mode.clone()
    } else {
        ours.mode.clone()
    };
    let directory = tempfile::tempdir()?;
    let ours_path = directory.path().join("ours");
    let base_path = directory.path().join("base");
    let theirs_path = directory.path().join("upstream");
    fs::write(&ours_path, &ours.data)?;
    fs::write(&base_path, &base.data)?;
    fs::write(&theirs_path, &theirs.data)?;
    let mut command = git::command(&repo.root);
    command.args([
        "merge-file",
        "-p",
        "-L",
        "overlay",
        "-L",
        "fork point",
        "-L",
        "base",
    ]);
    if style != "merge" {
        command.arg(format!("--{style}"));
    }
    let output = git::output(command.arg(ours_path).arg(base_path).arg(theirs_path), None)?;
    let code = output
        .status
        .code()
        .context("Git merge-file was interrupted")?;
    ensure!(
        (0..=127).contains(&code),
        "Git merge-file failed\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = Snapshot {
        mode,
        data: output.stdout,
    };
    Ok(if code == 0 {
        Merge::Clean(snapshot)
    } else {
        Merge::Conflict(snapshot)
    })
}

fn overlay_input(
    repo: &Repo,
    path: &str,
    current: &Snapshot,
    theirs: Option<&Snapshot>,
) -> Result<Snapshot> {
    if let Some(saved) = repo.indexed(path, true)?
        && saved != *current
        && (Some(current) == theirs || Some(current) == repo.indexed(path, false)?.as_ref())
    {
        return Ok(saved);
    }
    Ok(current.clone())
}

fn plan(repo: &Repo, path: &str, entry: &Entry, style: &str) -> Result<Planned> {
    let before = repo.work(path)?;
    let theirs = upstream(repo, path)?;
    let action = if entry.kind == Kind::Delete {
        match theirs {
            None => Action::Drop,
            Some(theirs) if entry.base.as_ref() != Some(&theirs) || before.is_some() => {
                Action::Update(
                    Entry {
                        kind: Kind::Delete,
                        base: Some(theirs),
                    },
                    None,
                )
            }
            Some(_) => Action::None,
        }
    } else if let Some(current) = before.as_ref() {
        let ours = overlay_input(repo, path, current, theirs.as_ref())?;
        match (entry.kind, theirs) {
            (Kind::Add, None) => Action::None,
            (Kind::Add, Some(theirs)) => Action::Pending(
                Pending {
                    situation: Situation::Collision,
                    ours: ours.clone(),
                    upstream: Some(theirs),
                },
                None,
            ),
            (Kind::Override, None) => Action::Pending(
                Pending {
                    situation: Situation::Deleted,
                    ours: ours.clone(),
                    upstream: None,
                },
                None,
            ),
            (Kind::Override, Some(theirs)) => {
                let base = entry
                    .base
                    .as_ref()
                    .context("override has no fork snapshot")?;
                if base == &theirs {
                    Action::None
                } else {
                    match merge(repo, base, &ours, &theirs, style)? {
                        Merge::Clean(merged) => Action::Update(
                            Entry {
                                kind: Kind::Override,
                                base: Some(theirs),
                            },
                            Some(merged),
                        ),
                        Merge::Conflict(merged) => Action::Pending(
                            Pending {
                                situation: Situation::Conflict,
                                ours: ours.clone(),
                                upstream: Some(theirs),
                            },
                            Some(merged),
                        ),
                        Merge::Binary => Action::Pending(
                            Pending {
                                situation: Situation::Binary,
                                ours: ours.clone(),
                                upstream: Some(theirs),
                            },
                            None,
                        ),
                    }
                }
            }
            (Kind::Delete, _) => bail!("invalid non-whiteout merge plan"),
        }
    } else {
        Action::Missing
    };
    Ok(Planned {
        path: path.to_owned(),
        before,
        action,
    })
}

fn plans(repo: &Repo, only: &[String], style: &str) -> Result<Vec<Planned>> {
    ensure!(
        matches!(style, "merge" | "diff3" | "zdiff3"),
        "invalid conflict style {style:?}"
    );
    let manifest = repo.manifest()?;
    for path in only {
        ensure!(
            manifest.entries.contains_key(path),
            "overlay does not own {path:?}"
        );
    }
    manifest
        .entries
        .iter()
        .filter(|(path, _)| only.is_empty() || only.contains(path))
        .map(|(path, entry)| plan(repo, path, entry, style))
        .collect()
}

fn finish(repo: &Repo) -> Result<()> {
    crate::ops::refresh_excludes(repo)?;
    fs::remove_file(repo.local("sync.json"))?;
    Ok(())
}

pub fn run(repo: &Repo, only: &[String], style: &str, dry_run: bool) -> Result<i32> {
    if dry_run && let Some(state) = read_state(repo)? {
        return preview_pending(&state, only);
    }
    repo.assert_no_sync()?;
    let plans = plans(repo, only, style)?;
    let changed = plans
        .iter()
        .any(|plan| !matches!(plan.action, Action::None | Action::Missing));
    let missing = plans
        .iter()
        .any(|plan| matches!(plan.action, Action::Missing));
    if dry_run {
        for plan in &plans {
            if !matches!(plan.action, Action::None) {
                crate::say!(
                    "{}\t{:?}",
                    if matches!(plan.action, Action::Missing) {
                        "missing"
                    } else {
                        "sync"
                    },
                    plan.path
                );
            }
        }
        return Ok(if changed || missing { 4 } else { 0 });
    }
    for plan in &plans {
        if matches!(plan.action, Action::Missing) {
            eprintln!(
                "missing {:?}; run `overgit apply` before syncing it",
                plan.path
            );
        }
    }
    if !changed {
        if missing {
            return Ok(4);
        }
        crate::say!("already up to date");
        return Ok(0);
    }
    let code = apply_plans(repo, plans)?;
    Ok(if code == 0 && missing { 4 } else { code })
}

fn preview_pending(state: &State, only: &[String]) -> Result<i32> {
    for path in only {
        ensure!(
            state.work.contains_key(path),
            "path {path:?} is outside the pending sync"
        );
    }
    if !state.completed {
        crate::say!("sync was interrupted; abort it before retrying");
        return Ok(4);
    }
    let mut pending = false;
    for path in state
        .pending
        .keys()
        .filter(|path| only.is_empty() || only.contains(path))
    {
        crate::say!("pending\t{path:?}; inspect it, then resolve or abort");
        pending = true;
    }
    Ok(if pending { 4 } else { 0 })
}

fn apply_plans(repo: &Repo, plans: Vec<Planned>) -> Result<i32> {
    let mut state = original_state(
        repo,
        plans
            .iter()
            .map(|plan| (plan.path.clone(), plan.before.clone()))
            .collect(),
    )?;
    for plan in &plans {
        if let Action::Pending(pending, _) = &plan.action {
            state.pending.insert(plan.path.clone(), pending.clone());
        }
    }
    save_state(repo, &state)?;
    let mut manifest = repo.manifest()?;
    for plan in plans {
        match plan.action {
            Action::None | Action::Missing => {}
            Action::Drop => {
                rescue_index(repo, &plan.path, None)?;
                manifest.entries.remove(&plan.path);
                repo.unstage_path(&plan.path)?;
                repo.skip(&plan.path, false)?;
                repo.replace(&plan.path, None, plan.before.is_some(), false)?;
            }
            Action::Update(entry, value) => {
                rescue_index(repo, &plan.path, value.as_ref())?;
                repo.skip(&plan.path, true)?;
                if let Some(value) = &value {
                    repo.stage(&plan.path, value)?;
                } else {
                    repo.unstage_path(&plan.path)?;
                }
                repo.replace(
                    &plan.path,
                    value.as_ref(),
                    plan.before.as_ref() != value.as_ref(),
                    false,
                )?;
                manifest.entries.insert(plan.path.clone(), entry);
                crate::say!("synced {:?}", plan.path);
            }
            Action::Pending(_, markers) => {
                repo.skip(&plan.path, true)?;
                if let Some(markers) = markers {
                    repo.replace(&plan.path, Some(&markers), true, false)?;
                }
                eprintln!(
                    "decision pending {:?}; edit and resolve, or use --keep / --take-upstream",
                    plan.path
                );
            }
        }
        repo.save_manifest(&manifest)?;
    }
    crate::ops::refresh_excludes(repo)?;
    state.completed = true;
    save_state(repo, &state)?;
    if state.pending.is_empty() {
        finish(repo)?;
        Ok(0)
    } else {
        Ok(3)
    }
}

pub fn continue_sync(repo: &Repo) -> Result<i32> {
    let Some(state) = read_state(repo)? else {
        crate::say!("sync is already finished");
        return Ok(0);
    };
    ensure!(
        state.completed,
        "sync was interrupted before applying every path; use `overgit sync --abort` and retry"
    );
    if !state.pending.is_empty() {
        for path in state.pending.keys() {
            eprintln!("still pending {path:?}");
        }
        return Ok(3);
    }
    finish(repo)?;
    Ok(0)
}

pub fn abort(repo: &Repo) -> Result<i32> {
    let state = read_state(repo)?.context("no sync is in progress")?;
    let index = index_path(repo)?;
    let _index_guard = crate::mutation::GitIndexLock::acquire(index.with_file_name("index.lock"))?;
    restore_shared_index(repo, state.shared_index.as_ref())?;
    restore_indexed_objects(repo, &state.indexed)?;
    if let Some(current) = crate::repo::read_snapshot(&repo.root.join(MANIFEST))?
        && current != state.manifest
    {
        repo.rescue(MANIFEST, &current)?;
    }
    for (path, before) in &state.work {
        let current = repo.work(path)?;
        repo.replace(path, before.as_ref(), current != *before, false)?;
    }
    atomic_write(&repo.root.join(MANIFEST), &state.manifest.data)?;
    atomic_write(&index, &state.index.data)?;
    restore_flags(repo, &state.base_flags)?;
    finish(repo)?;
    crate::say!("sync aborted");
    Ok(0)
}

fn markers_remain(data: &[u8]) -> bool {
    data.split(|byte| *byte == b'\n').any(|line| {
        [
            b"<<<<<<<".as_slice(),
            b"=======".as_slice(),
            b">>>>>>>".as_slice(),
        ]
        .iter()
        .any(|marker| {
            line.starts_with(marker)
                && (line.len() == 7 || line.get(7).is_some_and(u8::is_ascii_whitespace))
        })
    })
}

fn valid_decision(pending: &Pending, decision: Option<&str>) -> bool {
    matches!(
        (&pending.situation, decision),
        (
            Situation::Conflict,
            None | Some("keep" | "take-upstream" | "drop")
        ) | (
            Situation::Binary | Situation::Deleted,
            Some("keep" | "take-upstream" | "drop")
        ) | (
            Situation::Collision,
            Some("adopt" | "take-upstream" | "drop")
        ) | (Situation::Whiteout, Some("keep"))
    )
}

fn resolved(
    repo: &Repo,
    path: &str,
    pending: &Pending,
    decision: Option<&str>,
    force: bool,
) -> Result<(Option<Snapshot>, Option<Situation>)> {
    match decision {
        None => {
            let work = repo.work(path)?.context("resolved file is missing")?;
            ensure!(
                force || !markers_remain(&work.data),
                "conflict markers remain at {path:?}; edit them or use --force"
            );
            Ok((Some(work), None))
        }
        Some("keep") if matches!(pending.situation, Situation::Whiteout) => Ok((None, None)),
        Some("keep") => Ok((Some(pending.ours.clone()), None)),
        Some("take-upstream" | "drop") => Ok((pending.upstream.clone(), None)),
        Some("adopt") => match merge(
            repo,
            &Snapshot {
                mode: pending.ours.mode.clone(),
                data: Vec::new(),
            },
            &pending.ours,
            pending
                .upstream
                .as_ref()
                .context("collision has no upstream snapshot")?,
            "merge",
        )? {
            Merge::Clean(value) => Ok((Some(value), None)),
            Merge::Conflict(value) => Ok((Some(value), Some(Situation::Conflict))),
            Merge::Binary => Ok((Some(pending.ours.clone()), Some(Situation::Binary))),
        },
        Some(_) => bail!("invalid resolve decision"),
    }
}

fn rescue_original(repo: &Repo, path: &str, snapshot: &Snapshot) -> Result<()> {
    repo.rescue(path, snapshot)
}

fn rescue_index(repo: &Repo, path: &str, value: Option<&Snapshot>) -> Result<()> {
    if let Some(indexed) = repo.indexed(path, true)?
        && Some(&indexed) != value
        && repo.committed(path)?.as_ref() != Some(&indexed)
    {
        repo.rescue(path, &indexed)?;
    }
    Ok(())
}

fn rescue_resolution(
    repo: &Repo,
    path: &str,
    pending: &Pending,
    value: Option<&Snapshot>,
    decision: Option<&str>,
) -> Result<()> {
    let current = repo.work(path)?;
    if decision.is_some() && current.as_ref() != value {
        repo.backup(path)?;
    }
    if Some(&pending.ours) != value
        && current.as_ref() != Some(&pending.ours)
        && repo.committed(path)?.as_ref() != Some(&pending.ours)
    {
        rescue_original(repo, path, &pending.ours)?;
    }
    Ok(())
}

fn resolution_state(repo: &Repo, paths: &[String], decision: Option<&str>) -> Result<State> {
    let existing = read_state(repo)?;
    let state = if let Some(state) = existing {
        state
    } else {
        ensure!(decision.is_some(), "no sync is in progress");
        let plans = plans(repo, paths, "merge")?;
        let mut state = original_state(
            repo,
            plans
                .iter()
                .map(|plan| (plan.path.clone(), plan.before.clone()))
                .collect(),
        )?;
        for plan in plans {
            let pending = match plan.action {
                Action::Pending(pending, _) => pending,
                Action::Update(entry, None)
                    if entry.kind == Kind::Delete && decision == Some("keep") =>
                {
                    Pending {
                        situation: Situation::Whiteout,
                        ours: entry
                            .base
                            .clone()
                            .context("whiteout has no base snapshot")?,
                        upstream: entry.base,
                    }
                }
                _ => bail!("path {:?} does not need a decision", plan.path),
            };
            state.pending.insert(plan.path, pending);
        }
        state
    };
    ensure!(
        state.completed || !repo.local("sync.json").exists(),
        "sync was interrupted before applying every path; use `overgit sync --abort` and retry"
    );
    Ok(state)
}

pub fn resolve(repo: &Repo, paths: &[String], decision: Option<&str>, force: bool) -> Result<i32> {
    ensure!(!paths.is_empty(), "resolve needs at least one path");
    let mut state = resolution_state(repo, paths, decision)?;
    state.completed = true;
    let mut values = BTreeMap::new();
    for path in paths {
        let pending = state
            .pending
            .get(path)
            .with_context(|| format!("path {path:?} is not waiting for resolution"))?;
        ensure!(
            valid_decision(pending, decision),
            "invalid decision for {path:?}; use the choices reported by sync"
        );
        values.insert(
            path.clone(),
            resolved(repo, path, pending, decision, force)?,
        );
    }
    save_state(repo, &state)?;
    let mut manifest = repo.manifest()?;
    for (path, (value, remaining)) in values {
        let pending = state
            .pending
            .get(&path)
            .context("resolution disappeared")?
            .clone();
        rescue_index(repo, &path, value.as_ref())?;
        if matches!(decision, Some("drop"))
            || (matches!(pending.situation, Situation::Deleted | Situation::Collision)
                && matches!(decision, Some("take-upstream")))
        {
            manifest.entries.remove(&path);
            repo.unstage_path(&path)?;
            repo.skip(&path, false)?;
        } else {
            let kind = if matches!(pending.situation, Situation::Whiteout) {
                Kind::Delete
            } else if pending.upstream.is_some() {
                Kind::Override
            } else {
                Kind::Add
            };
            let entry = Entry {
                kind,
                base: pending.upstream.clone(),
            };
            manifest.entries.insert(path.clone(), entry);
            if let Some(value) = &value {
                repo.stage(&path, value)?;
            }
            repo.skip(&path, true)?;
        }
        rescue_resolution(repo, &path, &pending, value.as_ref(), decision)?;
        repo.replace(&path, value.as_ref(), false, false)?;
        if let Some(situation) = remaining {
            state.pending.insert(
                path.clone(),
                Pending {
                    situation,
                    ..pending
                },
            );
        } else {
            state.pending.remove(&path);
        }
        repo.save_manifest(&manifest)?;
        save_state(repo, &state)?;
    }
    crate::ops::refresh_excludes(repo)?;
    if state.pending.is_empty() {
        finish(repo)?;
        Ok(0)
    } else {
        Ok(3)
    }
}

pub(crate) fn pending_paths(repo: &Repo) -> Result<Vec<String>> {
    Ok(read_state(repo)?.map_or_else(Vec::new, |state| state.pending.into_keys().collect()))
}

pub fn inspect(repo: &Repo, path: &str) -> Result<i32> {
    let state = read_state(repo)?.context("no sync is in progress")?;
    let pending = state
        .pending
        .get(path)
        .with_context(|| format!("path {path:?} is not pending"))?;
    let report = serde_json::json!({
        "path": path,
        "situation": pending.situation,
        "ours": pending.ours,
        "upstream": pending.upstream,
        "worktree": repo.work(path)?,
        "saved_overlay": repo.indexed(path, true)?,
    });
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &report)?;
    writeln!(stdout)?;
    Ok(0)
}
