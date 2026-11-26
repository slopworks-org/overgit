use crate::{
    RecoveryCommand,
    model::{Entry, Kind, MANIFEST, Snapshot, validate_path},
    mutation,
    repo::{Repo, ensure_not_symlink, read_snapshot},
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

const PREVIEW_BYTES: usize = 256;
const METADATA_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Copy)]
enum Format {
    Snapshot,
    Directory,
    Pull,
}
impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Directory => "directory",
            Self::Pull => "pull-archive",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    timestamp_unix: u64,
    original_path: String,
    reason: String,
    original_type: String,
    original_mode: String,
    content_location: String,
}
struct Selected {
    snapshot: Snapshot,
    original: String,
    format: Format,
    metadata_only: bool,
    details: Value,
}
struct Plan {
    destination: String,
    current: Option<Snapshot>,
    changed: bool,
    owned: bool,
    base_staged_matches: Option<bool>,
    overlay_staged_matches: Option<bool>,
}

pub fn run(repo: &Repo, command: &RecoveryCommand) -> Result<i32> {
    dispatch(repo, command)
        .map_err(|error| anyhow::anyhow!("{}", format!("{error:#}").escape_default()))
}
fn dispatch(repo: &Repo, command: &RecoveryCommand) -> Result<i32> {
    match command {
        RecoveryCommand::List { porcelain } => list(repo, *porcelain),
        RecoveryCommand::Inspect {
            id,
            path,
            porcelain,
        } => inspect(repo, id, path.as_deref(), *porcelain),
        RecoveryCommand::Restore {
            id,
            path,
            to,
            dry_run,
            porcelain,
        } => {
            let _lock = if *dry_run { None } else { Some(repo.lock()?) };
            restore(
                repo,
                id,
                path.as_deref(),
                to.as_deref(),
                *dry_run,
                *porcelain,
            )
        }
    }
}
fn storage(repo: &Repo) -> Result<PathBuf> {
    let path = repo.local("backups");
    ensure_not_symlink(&path)?;
    Ok(path)
}
fn entry(repo: &Repo, id: &str) -> Result<PathBuf> {
    ensure!(
        !id.is_empty() && !id.contains('/') && !id.contains('\0') && !matches!(id, "." | ".."),
        "recovery ID must be an exact entry name without path separators"
    );
    let path = storage(repo)?.join(id);
    ensure_not_symlink(&path)?;
    ensure!(
        fs::symlink_metadata(&path)?.is_dir(),
        "selected recovery entry is not a directory"
    );
    Ok(path)
}
fn plain(path: &Path, limit: Option<u64>) -> Result<Vec<u8>> {
    ensure_not_symlink(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "expected an ordinary recovery file at {}",
        path.display()
    );
    if let Some(limit) = limit {
        ensure!(
            metadata.len() <= limit,
            "recovery metadata exceeds {limit} bytes"
        );
    }
    Ok(fs::read(path)?)
}
fn ordinary(path: &Path, directory: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "recovery metadata symlink at {}",
                path.display()
            );
            Ok(if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn format(path: &Path) -> Result<Format> {
    let snapshot = ordinary(&path.join("snapshot.json"), false)?;
    let directory = ordinary(&path.join("content"), true)?;
    let pull = ordinary(&path.join("tree"), true)?;
    ensure!(
        usize::from(snapshot) + usize::from(directory) + usize::from(pull) == 1,
        "ambiguous, unsupported or incomplete recovery entry; preserve it for manual inspection"
    );
    if snapshot {
        ensure!(
            ordinary(&path.join("content"), false)?,
            "snapshot content is missing or not an ordinary file"
        );
        Ok(Format::Snapshot)
    } else if directory {
        ensure!(
            ordinary(&path.join("path"), false)?,
            "directory recovery path is missing"
        );
        Ok(Format::Directory)
    } else {
        ensure!(
            ordinary(&path.join("session.json"), false)?,
            "archived pull session is missing"
        );
        Ok(Format::Pull)
    }
}

fn metadata(path: &Path) -> Result<Metadata> {
    let value: Metadata =
        serde_json::from_slice(&plain(&path.join("recovery.json"), Some(METADATA_LIMIT))?)?;
    let recorded = String::from_utf8(plain(&path.join("path"), Some(METADATA_LIMIT))?)?;
    ensure!(
        recorded == value.original_path,
        "recorded original paths disagree"
    );
    ensure!(
        value.content_location == "content",
        "unexpected recovery content location"
    );
    Ok(value)
}
fn metadata_label(path: &str) -> bool {
    validate_path(path).is_err()
        || matches!(
            path,
            "overlay index before rollback"
                | "detach marker before rollback"
                | "invalid indexed manifest"
        )
        || path.starts_with("sharedindex.")
}
fn selection(repo: &Repo, id: &str, relative: Option<&str>) -> Result<Selected> {
    let path = entry(repo, id)?;
    let format = format(&path)?;
    match format {
        Format::Snapshot => {
            ensure!(relative.is_none(), "snapshot entries do not accept --path");
            let meta = metadata(&path)?;
            let snapshot: Snapshot =
                serde_json::from_slice(&plain(&path.join("snapshot.json"), None)?)?;
            snapshot.validate()?;
            ensure!(
                snapshot.data == plain(&path.join("content"), None)?,
                "snapshot and content bytes disagree"
            );
            let kind = if snapshot.mode == "120000" {
                "symlink"
            } else {
                "file"
            };
            ensure!(
                meta.original_type == kind && meta.original_mode == snapshot.mode,
                "snapshot type or mode disagrees with recovery metadata"
            );
            Ok(Selected {
                metadata_only: metadata_label(&meta.original_path),
                details: json!({"timestamp_unix":meta.timestamp_unix,"reason":meta.reason}),
                original: meta.original_path,
                snapshot,
                format,
            })
        }
        Format::Directory | Format::Pull => {
            let relative = relative.context("select a file or symlink leaf with --path; whole archives are not restored automatically")?;
            ensure!(
                relative != MANIFEST || matches!(format, Format::Pull),
                "reserved archive path"
            );
            if relative != MANIFEST {
                validate_path(relative)?;
            }
            let (root, original) = match format {
                Format::Directory => {
                    let meta = metadata(&path)?;
                    ensure!(
                        meta.original_type == "directory",
                        "saved obstruction is not a directory"
                    );
                    (
                        path.join("content"),
                        format!("{}/{relative}", meta.original_path),
                    )
                }
                Format::Pull => (path.join("tree"), relative.to_owned()),
                Format::Snapshot => bail!("snapshot entries do not select archive leaves"),
            };
            let leaf = root.join(relative);
            ensure_not_symlink(leaf.parent().context("archive leaf has no parent")?)?;
            let snapshot = read_snapshot(&leaf)?.context("selected archive leaf is missing")?;
            snapshot.validate()?;
            Ok(Selected {
                metadata_only: metadata_label(&original),
                details: Value::Null,
                original,
                snapshot,
                format,
            })
        }
    }
}
fn list(repo: &Repo, porcelain: bool) -> Result<i32> {
    let directory = storage(repo)?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut items = Vec::new();
    if let Some(entries) = entries {
        for item in entries {
            let item = item?;
            let name = item.file_name();
            let id = name.to_string_lossy().into_owned();
            let description = if name.to_str().is_none() {
                Err(anyhow::anyhow!(
                    "non-UTF-8 recovery IDs require manual inspection"
                ))
            } else {
                entry(repo, &id).and_then(|path| format(&path))
            };
            items.push(match description {
                Ok(kind) => json!({"id":id,"format":kind.name(),"error":null}),
                Err(error) => json!({"id":id,"format":"unsupported","error":format!("{error:#}")}),
            });
        }
    }
    items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let report = json!({"version":1,"location":".overgit/local/backups","entries":items,"note":"entry formats only; payloads have not been validated"});
    if porcelain {
        output(&report)?;
    } else {
        for item in &items {
            crate::say!(
                "{}  {}{}",
                escaped(item["id"].as_str().unwrap_or("?")),
                item["format"].as_str().unwrap_or("?"),
                item["error"]
                    .as_str()
                    .map_or_else(String::new, |error| format!("  {}", escaped(error)))
            );
        }
        if items.is_empty() {
            crate::say!("no local recovery entries");
        }
    }
    Ok(0)
}
fn inspect(repo: &Repo, id: &str, relative: Option<&str>, porcelain: bool) -> Result<i32> {
    let path = entry(repo, id)?;
    let kind = format(&path)?;
    if relative.is_none() && !matches!(kind, Format::Snapshot) {
        let report = archive_summary(&path, id, kind)?;
        return show(&report, porcelain);
    }
    let selected = selection(repo, id, relative)?;
    let mut report = selected_report(id, relative, &selected);
    match plan(repo, &selected, None) {
        Ok(plan) => report["restore"] = plan_report(&plan),
        Err(error) => report["restore"] = json!({"eligible":false,"reason":format!("{error:#}")}),
    }
    show(&report, porcelain)
}
fn archive_summary(path: &Path, id: &str, kind: Format) -> Result<Value> {
    let (directory, original, details) = if matches!(kind, Format::Directory) {
        let meta = metadata(path)?;
        ensure!(
            meta.original_type == "directory",
            "saved obstruction is not a directory"
        );
        (
            path.join("content"),
            Some(meta.original_path),
            json!({"mode":meta.original_mode,"timestamp_unix":meta.timestamp_unix,"reason":meta.reason}),
        )
    } else {
        (path.join("tree"), None, Value::Null)
    };
    ensure_not_symlink(&directory)?;
    let mut children = Vec::new();
    for child in fs::read_dir(&directory)? {
        let child = child?;
        let kind = child.file_type()?;
        let name = child.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == ".overgit" {
            continue;
        }
        children.push(json!({"name":name,"type":if kind.is_symlink(){"symlink"}else if kind.is_dir(){"directory"}else if kind.is_file(){"file"}else{"special"}}));
    }
    children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(
        json!({"version":1,"id":id,"format":kind.name(),"original_path":original,"metadata":details,"children":children,"note":"choose --path relative to this tree to inspect or restore one file or symlink; bulk restoration is unsupported"}),
    )
}
fn plan(repo: &Repo, selected: &Selected, destination: Option<&Path>) -> Result<Plan> {
    repo.require_overlay()?;
    repo.assert_idle()?;
    repo.assert_attached()?;
    mutation::assert_clear(repo)?;
    repo.assert_no_sync()?;
    ensure!(
        crate::pull::state_summary(repo)?.is_none(),
        "finish or abort the pending pull before restoring a working file"
    );
    let manifest = repo.manifest()?;
    let destination = if let Some(destination) = destination {
        repo.path(destination)?
    } else {
        ensure!(
            !selected.metadata_only,
            "this recovery entry records metadata or a label; use --to with an ordinary project path to extract it safely"
        );
        repo.safe_path(&selected.original)?;
        selected.original.clone()
    };
    let owned = manifest.entries.contains_key(&destination);
    let whiteout = if let Some(entry) = manifest.entries.get(&destination) {
        entry.kind == Kind::Delete
    } else {
        manifest.opaque_directory(&destination).is_some()
            && repo.index_file(&destination, false)?.is_some()
    };
    ensure!(
        !whiteout,
        "destination is an overlay whiteout; decide its ownership with existing restore/add commands first, or select another --to path"
    );
    if !owned {
        let mut proposed = manifest.clone();
        proposed.entries.insert(
            destination.clone(),
            Entry {
                kind: Kind::Add,
                base: None,
            },
        );
        proposed.validate()?;
    }
    repo.validate_destination_components(&destination)?;
    let current = repo.work(&destination)?;
    let changed = current.as_ref() != Some(&selected.snapshot);
    Ok(Plan {
        base_staged_matches: repo
            .indexed(&destination, false)?
            .map(|value| value == selected.snapshot),
        overlay_staged_matches: repo
            .indexed(&destination, true)?
            .map(|value| value == selected.snapshot),
        destination,
        current,
        changed,
        owned,
    })
}
fn restore(
    repo: &Repo,
    id: &str,
    relative: Option<&str>,
    destination: Option<&Path>,
    dry_run: bool,
    porcelain: bool,
) -> Result<i32> {
    let selected = selection(repo, id, relative)?;
    let plan = plan(repo, &selected, destination)?;
    let paths = [plan.destination.clone()];
    if plan.changed {
        if dry_run {
            mutation::preflight(repo, &paths)?;
        } else {
            mutation::run(repo, &paths, || {
                repo.replace(
                    &plan.destination,
                    Some(&selected.snapshot),
                    plan.current.is_some(),
                    false,
                )
            })?;
        }
    }
    let mut report = selected_report(id, relative, &selected);
    report["restore"] = plan_report(&plan);
    report["dry_run"] = dry_run.into();
    report["result"] = if !plan.changed {
        "unchanged"
    } else if dry_run {
        "would-restore"
    } else {
        "restored"
    }
    .into();
    show(&report, porcelain)?;
    Ok(if dry_run && plan.changed { 4 } else { 0 })
}
fn snapshot_report(snapshot: &Snapshot) -> Value {
    let preview: String = snapshot
        .data
        .iter()
        .take(PREVIEW_BYTES)
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect();
    json!({"type":if snapshot.mode=="120000"{"symlink"}else{"file"},"mode":snapshot.mode,"bytes":snapshot.data.len(),"preview_escaped":preview,"preview_truncated":snapshot.data.len()>PREVIEW_BYTES})
}
fn selected_report(id: &str, relative: Option<&str>, selected: &Selected) -> Value {
    json!({"version":1,"id":id,"path":relative,"format":selected.format.name(),"original_path":selected.original,"requires_destination":selected.metadata_only,"metadata":selected.details,"saved":snapshot_report(&selected.snapshot)})
}
fn plan_report(plan: &Plan) -> Value {
    json!({"eligible":true,"destination":plan.destination,"would_change":plan.changed,"would_rescue_current":plan.changed && plan.current.is_some(),"current":plan.current.as_ref().map(snapshot_report),"owned":plan.owned,"base_staged_matches_saved":plan.base_staged_matches,"overlay_staged_matches_saved":plan.overlay_staged_matches,"effect":"working file only; both indexes and ownership remain unchanged","next":if plan.owned{"use add --force and commit if you want to save the restored work in the overlay"}else{"destination is not overlay-owned; base Git may include it until you explicitly choose ownership"}})
}
fn escaped(text: &str) -> String {
    text.escape_default().to_string()
}
fn output(report: &Value) -> Result<()> {
    // Encode non-ASCII characters too, so machine output is safe on a terminal even
    // for hostile filenames or Unicode direction controls. Preview bytes are escaped.
    let encoded = serde_json::to_string_pretty(report)?;
    let mut stdout = io::stdout().lock();
    for character in encoded.chars() {
        if character <= '~' {
            write!(stdout, "{character}")?;
        } else {
            let mut units = [0; 2];
            for unit in character.encode_utf16(&mut units) {
                write!(stdout, "\\u{unit:04x}")?;
            }
        }
    }
    writeln!(stdout)?;
    Ok(())
}
fn show(report: &Value, porcelain: bool) -> Result<i32> {
    if porcelain {
        output(report)?;
    } else {
        // One structured preview in both modes avoids a raw-content display path.
        if !crate::QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            output(report)?;
        }
    }
    Ok(0)
}
