use crate::{
    git,
    model::{Kind, Manifest},
    ops,
    repo::Repo,
};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write as _},
    path::PathBuf,
};

pub fn list(repo: &Repo, kind: Option<&str>, porcelain: bool) -> Result<i32> {
    let manifest = repo.manifest()?;
    let mut out = io::stdout().lock();
    let files = manifest
        .entries
        .iter()
        .map(|(path, entry)| (path, entry.kind.name()));
    let dirs = manifest.opaque.iter().map(|dir| (dir, "opaque"));
    for (path, name) in files.chain(dirs) {
        if kind.is_some_and(|kind| name != kind) {
            continue;
        }
        if porcelain {
            write!(out, "{name}\t{path}\0")?;
        } else {
            crate::say!("{name:<8} {path:?}");
        }
    }
    Ok(0)
}
pub fn which(repo: &Repo, paths: &[PathBuf], porcelain: bool) -> Result<i32> {
    let manifest = repo.manifest()?;
    let index = repo.index(false)?;
    let mut outside = false;
    let mut out = io::stdout().lock();
    for input in paths {
        let path = repo.path(input)?;
        let (owner, kind) = if let Some(entry) = manifest.entries.get(&path) {
            ("overlay", entry.kind.name())
        } else if manifest.opaque.contains(&path)
            || (index.contains_key(&path) && manifest.opaque_directory(&path).is_some())
        {
            ("overlay", "opaque")
        } else {
            outside = true;
            (
                "base",
                if index.contains_key(&path) {
                    "tracked"
                } else {
                    "untracked"
                },
            )
        };
        if porcelain {
            write!(out, "{owner}\t{kind}\t{path}\0")?;
        } else {
            crate::say!("{path:?}  {owner} {kind}");
        }
    }
    Ok(i32::from(outside))
}
fn reference(repo: &Repo, overlay: bool, args: &[&str]) -> Result<Option<String>> {
    let mut cmd = if overlay {
        repo.overlay_cmd()
    } else {
        repo.base_cmd()
    };
    let output = git::output(cmd.args(args), None)?;
    if output.status.success() {
        return Ok(Some(git::text(output.stdout)?));
    }
    ensure!(
        matches!(output.status.code(), Some(1 | 128)),
        "Git reference probe failed with {}",
        output.status
    );
    Ok(None)
}
fn repository_status(repo: &Repo, overlay: bool) -> Result<Value> {
    let branch = reference(repo, overlay, &["symbolic-ref", "--short", "-q", "HEAD"])?;
    let head = reference(repo, overlay, &["rev-parse", "--verify", "HEAD"])?;
    let upstream = reference(
        repo,
        overlay,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )?;
    let (ahead, behind) = if upstream.is_some() && head.is_some() {
        let bytes = if overlay {
            repo.overlay(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?
        } else {
            repo.base(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?
        };
        let text = git::text(bytes)?;
        let mut counts = text.split_whitespace();
        (
            counts.next().unwrap_or("0").parse::<u64>()?,
            counts.next().unwrap_or("0").parse::<u64>()?,
        )
    } else {
        (0, 0)
    };
    Ok(json!({"branch":branch,"head":head,"upstream":upstream,"ahead":ahead,"behind":behind}))
}
fn owned_status(repo: &Repo, manifest: &Manifest) -> Result<Vec<Value>> {
    let repo = &repo.with_layout(repo.managed_files(manifest)?.keys());
    let overlay_index = repo.index(true)?;
    let staged_bytes = repo.overlay(["diff", "--cached", "--name-only", "-z"])?;
    let staged: BTreeSet<_> = staged_bytes
        .split(|byte| *byte == 0)
        .filter_map(|path| std::str::from_utf8(path).ok())
        .collect();
    let mut files = Vec::new();
    for (path, entry) in &manifest.entries {
        let base = crate::sync::upstream(repo, path)?;
        let indexed = if entry.kind == Kind::Delete {
            None
        } else {
            repo.indexed(path, true)?
        };
        let work = repo.work(path);
        let blocked = work.is_err();
        let work = work.ok().flatten();
        let upstream = if entry.kind == Kind::Add {
            if base.is_some() { "added" } else { "same" }
        } else if base.is_none() {
            "deleted"
        } else if base == entry.base {
            "same"
        } else {
            "changed"
        };
        files.push(json!({"path":path,"kind":entry.kind.name(),"staged":staged.contains(path.as_str()),"dirty":work!=indexed,"missing":work.is_none() && entry.kind != Kind::Delete,"blocked":blocked,"upstream":upstream,"saved":overlay_index.contains_key(path)}));
    }
    Ok(files)
}

fn base_changes(
    repo: &Repo,
    manifest: &Manifest,
    warnings: &mut Vec<String>,
) -> Result<Vec<Value>> {
    let bytes = repo.base(["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut changes = Vec::new();
    let mut records = bytes.split(|byte| *byte == 0).filter(|row| !row.is_empty());
    while let Some(record) = records.next() {
        ensure!(record.len() >= 3, "invalid Git status record");
        let xy = std::str::from_utf8(&record[..2])?;
        let raw_path = &record[3..];
        let path = String::from_utf8_lossy(raw_path);
        let original = if xy.contains(['R', 'C']) {
            Some(
                String::from_utf8_lossy(records.next().context("Git rename has no source record")?)
                    .into_owned(),
            )
        } else {
            None
        };
        if manifest.entries.contains_key(path.as_ref())
            || (xy != "??" && manifest.opaque_directory(&path).is_some())
        {
            warnings.push(format!(
                "base can see owned path {path:?} ({xy}); run doctor"
            ));
            continue;
        }
        if path.starts_with(".overgit/") {
            continue;
        }
        changes.push(json!({"status":xy,"path":path,"original":original,"path_utf8":std::str::from_utf8(raw_path).is_ok(),"path_bytes":STANDARD.encode(raw_path)}));
    }
    Ok(changes)
}

fn pull_blocked(repo: &Repo, manifest: &Manifest, base: &Value) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    if base["behind"].as_u64().unwrap_or(0) > 0 {
        let bytes = repo.base(["diff", "--name-only", "-z", "HEAD..@{upstream}"])?;
        for path in bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let Ok(path) = std::str::from_utf8(path) else {
                continue;
            };
            if manifest.manages_tracked(path) {
                paths.push(path.to_owned());
            }
        }
    }
    Ok(paths)
}

fn render_status(repo: &Repo, report: &Value, short: bool, all: bool) -> Result<()> {
    let base = &report["base"];
    let overlay = &report["overlay"];
    let files = report["files"]
        .as_array()
        .context("status files are missing")?;
    if !short {
        crate::say!(
            "base {}  overlay {}",
            base["branch"].as_str().unwrap_or("(detached)"),
            overlay["branch"].as_str().unwrap_or("(detached)")
        );
        crate::say!(
            "overlay ahead {} behind {}",
            overlay["ahead"],
            overlay["behind"]
        );
        if report["detached"] == true {
            crate::say!("overlay detached; run attach when base operations are done");
        }
        if !report["detach"].is_null() {
            crate::say!(
                "detach complete {}  started {}  restored {} paths",
                report["detach"]["complete"],
                report["detach"]["started_unix_secs"],
                report["detach"]["restored"].as_array().map_or(0, Vec::len)
            );
        }
        if !report["pull"].is_null() {
            crate::say!("isolated pull pending; use pull --status, --continue or --abort");
        }
        if report["sync_in_progress"] == true {
            crate::say!(
                "{}",
                if crate::sync::is_interrupted(repo) {
                    "sync was interrupted; run `overgit sync --abort` before retrying"
                } else {
                    "sync in progress; resolve, then sync --continue or --abort"
                }
            );
        }
    }
    let opaque = report["opaque"]
        .as_array()
        .context("opaque directories are missing")?;
    for dir in opaque {
        crate::say!("opaque   {:?}", dir.as_str().unwrap_or("?"));
    }
    for file in files {
        if !all
            && file["staged"] == false
            && file["dirty"] == false
            && file["missing"] == false
            && file["blocked"] == false
            && file["upstream"] == "same"
        {
            continue;
        }
        crate::say!(
            "{:<8} {} {} {:<7} {:?}",
            file["kind"].as_str().unwrap_or("?"),
            if file["staged"] == true { "S" } else { "." },
            if file["missing"] == true {
                "!"
            } else if file["dirty"] == true {
                "M"
            } else {
                "."
            },
            file["upstream"].as_str().unwrap_or("?"),
            file["path"].as_str().unwrap_or("?")
        );
    }
    for change in report["base_changes"]
        .as_array()
        .context("base changes are missing")?
    {
        crate::say!(
            "base {} {:?}",
            change["status"].as_str().unwrap_or("?"),
            change["path"].as_str().unwrap_or("?")
        );
    }
    render_pull_blocked(report)?;
    for path in report["sync_pending"]
        .as_array()
        .context("sync paths are missing")?
    {
        crate::say!("sync pending {:?}", path.as_str().unwrap_or("?"));
    }
    render_diagnostics(report)?;
    if !short {
        crate::say!(
            "{} owned paths; {} base worktrees",
            files.len() + opaque.len(),
            ops::worktree_roots(repo)?.len()
        );
    }
    Ok(())
}

fn render_pull_blocked(report: &Value) -> Result<()> {
    const MAX_LINES: usize = 10;
    let blocked = report["pull_blocked"]
        .as_array()
        .context("pull warnings are missing")?;
    for path in blocked.iter().take(MAX_LINES) {
        crate::say!(
            "base upstream touches owned path {:?}; detach before pulling",
            path.as_str().unwrap_or("?")
        );
    }
    if blocked.len() > MAX_LINES {
        crate::say!(
            "... and {} more owned paths touched by base upstream; detach before pulling",
            blocked.len() - MAX_LINES
        );
    }
    Ok(())
}

fn render_diagnostics(report: &Value) -> Result<()> {
    for warning in report["warnings"]
        .as_array()
        .context("status warnings are missing")?
    {
        crate::say!("warning {}", warning.as_str().unwrap_or("?"));
    }
    for finding in report["problems"]
        .as_array()
        .context("status diagnostics are missing")?
        .iter()
        .filter(|finding| finding["severity"] == "info" && finding["code"] != "opaque-directory")
    {
        crate::say!(
            "info {} {}{}\n  {}",
            finding["code"].as_str().unwrap_or("?"),
            finding["message"].as_str().unwrap_or("?"),
            finding["path"]
                .as_str()
                .map_or_else(String::new, |path| format!(" at {path:?}")),
            finding["hint"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

pub fn status(repo: &Repo, short: bool, all: bool, porcelain: bool) -> Result<i32> {
    let inspection = repo.for_inspection();
    let repo = &inspection;
    let manifest = repo.manifest()?;
    let files = owned_status(repo, &manifest)?;
    let base = repository_status(repo, false)?;
    let overlay = repository_status(repo, true)?;
    let mut warnings = Vec::new();
    let problems = match crate::doctor::diagnose(repo) {
        Ok(problems) => problems,
        Err(error) => {
            warnings.push(format!("cannot complete diagnosis ({error:#})"));
            Vec::new()
        }
    };
    warnings.extend(
        problems
            .iter()
            .filter(|problem| {
                problem.severity == crate::doctor::Severity::Warning
                    && problem.code != "base-pull-owned"
            })
            .map(|problem| match &problem.path {
                Some(path) => format!("{} {path:?} {}", problem.code, problem.message),
                None => format!("{} {}", problem.code, problem.message),
            }),
    );
    let changes = base_changes(repo, &manifest, &mut warnings)?;
    let blocked = pull_blocked(repo, &manifest, &base)?;
    let pending = match crate::sync::pending_paths(repo) {
        Ok(paths) => paths,
        Err(error) => {
            warnings.push(format!("cannot read pending sync ({error})"));
            Vec::new()
        }
    };
    let pull = match crate::pull::state_summary(repo) {
        Ok(state) => state,
        Err(error) => Some(json!({"error":error.to_string()})),
    };
    let detach = if repo.detached() {
        Some(
            std::fs::read(repo.local("detached.json"))
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?))
                .unwrap_or_else(|error| json!({"error":error.to_string()})),
        )
    } else {
        None
    };
    let report = json!({"version":2,"base":base,"overlay":overlay,"detached":repo.detached(),"detach":detach,"pull":pull,"sync_in_progress":repo.local("sync.json").exists(),"sync_pending":pending,"files":files,"opaque":manifest.opaque,"base_changes":changes,"pull_blocked":blocked,"warnings":warnings,"problems":problems});
    if porcelain {
        let mut out = io::stdout().lock();
        serde_json::to_writer_pretty(&mut out, &report)?;
        writeln!(out)?;
    } else {
        render_status(repo, &report, short, all)?;
    }
    Ok(0)
}
