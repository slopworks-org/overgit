use crate::{mutation, ops, repo::Repo};
use anyhow::{Context as _, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, TryLockError},
    os::unix::prelude::{CommandExt as _, ExitStatusExt as _, OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(PartialEq, Eq)]
enum Hooks {
    ReadOnly,
    None,
    Commit,
    DetachAttach,
    IndexedPatch,
}
use Hooks::{DetachAttach, IndexedPatch, ReadOnly};
const TRACK_STAT: &str = "core.ignoreStat=false";
const RM_OPTIONS: &str = "n,dry-run q,quiet cached f,force r ignore-unmatch sparse pathspec-from-file= pathspec-file-nul";
const APPLY_OPTIONS: &str = "exclude= include= p= no-add stat numstat summary check index N,intent-to-add cached unsafe-paths apply 3,3way ours theirs union build-fake-ancestor= z C= whitespace= ignore-space-change ignore-whitespace R,reverse unidiff-zero reject allow-overlap v,verbose q,quiet inaccurate-eof recount directory= allow-empty binary allow-binary-replacement";
fn rule(cwd: &Path, globals: &[OsString], root: &Path, args: &[OsString]) -> Result<Hooks> {
    let option = args.get(1).and_then(|arg| arg.to_str());
    let spec = match args[0].to_str().unwrap_or_default() {
        _ if option == Some("-h") => return Ok(ReadOnly),
        "mv" => "v,verbose f,force n,dry-run k sparse",
        "rm" => RM_OPTIONS,
        "clean" => "q,quiet n,dry-run f,force i,interactive d e,exclude= x X",
        "apply" => APPLY_OPTIONS,
        "commit" => return Ok(Hooks::Commit),
        "stash" if matches!(option, Some("list" | "show")) => return Ok(ReadOnly),
        "status" | "diff" | "log" | "show" | "blame" | "grep" | "rev-parse" | "ls-files"
        | "help" => return Ok(ReadOnly),
        "pull" | "merge" | "rebase" | "checkout" | "switch" | "reset" | "cherry-pick"
        | "revert" | "stash" | "restore" | "bisect" | "am" => return Ok(DetachAttach),
        _ => return Ok(Hooks::None),
    };
    let spec = format!("options\n--\n{} \n", spec.replace(' ', " \n"));
    let mut parse = git(cwd, globals);
    parse.args(["rev-parse", "--parseopt", "--stuck-long", "--"]);
    let out = crate::git::output(parse.args(&args[1..]), Some(spec.as_bytes()))?;
    let parsed = out.stdout.strip_prefix(b"set -- ");
    if parsed.is_none() && !out.stdout.is_empty() {
        return Ok(ReadOnly); // Git printed help or completion.
    }
    let words = alias_words(parsed.unwrap_or_default()).context("invalid Git option list")?;
    let flags = words.split(|w| w == "--").next().unwrap_or_default();
    let last = |name: &str| flags.iter().rposition(|arg| arg == name);
    let on = |name: &str| last(&format!("--{name}")) > last(&format!("--no-{name}"));
    let fake_index = b"--build-fake-ancestor=";
    let ancestor = flags.iter().any(|w| w.as_bytes().starts_with(fake_index));
    let report = ["stat", "numstat", "check", "summary"].into_iter().any(on);
    if on("dry-run") || (report && !on("apply") && !ancestor) {
        return Ok(ReadOnly);
    }
    if args[0] == "clean" {
        let mut clean = git(cwd, globals);
        clean.args(["clean", "-ndx"]);
        let force = |w: &&OsString| matches!(w.as_bytes(), b"--force" | b"--no-force");
        clean.args(flags.iter().filter(force));
        if parsed.is_none() {
            // Unknown options may consume operands. Overestimate force on this error path.
            let bytes = args.iter().flat_map(|w| w.as_bytes());
            clean.args(bytes.filter(|&&b| b == b'f').take(2).map(|_| "-f"));
        }
        let probe = clean.arg("--").arg(root.join(".overgit")).output()?;
        crate::git::check(&probe)?;
        let refusal = "refusing clean flags that could delete .overgit. Use a single -f (`overgit base -c clean.requireForce=true clean -f`) with your other options and paths.";
        ensure!(probe.stdout.is_empty(), "{refusal}");
    }
    let index = on("index") || on("3way");
    Ok(if index { IndexedPatch } else { DetachAttach })
}
fn subcommand(args: &[OsString]) -> Option<usize> {
    let mut words = args.iter().enumerate();
    while let Some((i, arg)) = words.next() {
        match arg.as_bytes() {
            b"-C" | b"-c" | b"--git-dir" | b"--work-tree" | b"--namespace" | b"--config-env"
            | b"--super-prefix" | b"--shallow-file" | b"--attr-source" => {
                words.next()?;
            }
            b"-h" | b"--help" | b"-v" | b"--version" | b"--list-cmds" | b"--info-path"
            | b"--man-path" | b"--html-path" | b"--exec-path" => return None,
            flag if flag.starts_with(b"--list-cmds=") => return None,
            [b'-', ..] => {}
            _ if args.get(i + 1).is_some_and(|word| word == "--help") => return None,
            _ => return Some(i),
        }
    }
    None
}
fn git(cwd: &Path, globals: &[OsString]) -> Command {
    let mut git = Command::new("git");
    git.current_dir(cwd).env_remove("GIT_CONFIG");
    git.args(globals).arg("--no-pager");
    git
}
fn listed(cwd: &Path, globals: &[OsString], args: &[&str], name: &[u8]) -> Result<bool> {
    let out = git(cwd, globals).args(args).output()?;
    crate::git::check(&out)?;
    Ok(out.stdout.split(|b| *b == b'\n').any(|line| line == name))
}
fn git_path(cwd: &Path, globals: &[OsString], option: &str) -> Result<Option<PathBuf>> {
    let out = git(cwd, globals).args(["rev-parse", option]).output()?;
    let path = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    Ok(out.status.success().then(|| OsStr::from_bytes(path).into()))
}

// Git alias quoting allows a backslash escape anywhere outside single quotes.
fn alias_words(value: &[u8]) -> Option<Vec<OsString>> {
    let mut words = vec![Vec::new()];
    let mut quote = None;
    let mut bytes = value.iter().copied().peekable();
    while let Some(c) = bytes.next() {
        let word = words.last_mut()?;
        match c {
            b'\\' if quote != Some(b'\'') => word.push(bytes.next()?),
            b'\'' | b'"' if quote.is_none() => quote = Some(c),
            _ if quote == Some(c) => quote = None,
            _ if quote.is_none() && c.is_ascii_whitespace() => {
                while bytes.next_if(u8::is_ascii_whitespace).is_some() {}
                words.push(Vec::new());
            }
            _ => word.push(c),
        }
    }
    let words = words.into_iter().map(OsString::from_vec).collect();
    quote.is_none().then_some(words)
}
// Config order decides between alias.NAME and Git's newer [alias "NAME"] command.
fn alias(cwd: &Path, globals: &[OsString], name: &[u8]) -> Result<Option<Vec<OsString>>> {
    let config = ["config", "-z", "--get-regexp", "^alias\\."];
    let out = git(cwd, globals).args(config).output()?;
    let mut rows = out.stdout.rsplit(|b| *b == 0);
    let dotted = name.contains(&b'.');
    let classic = [b"alias.", name.to_ascii_lowercase().as_slice(), b"\n"].concat();
    let group = [b"alias.", name, b".command\n"].concat();
    let sections = (dotted || rows.clone().any(|r| r.starts_with(&group)))
        && listed(cwd, globals, &["help", "--config"], b"alias.*.command")?;
    let classic = (!sections || !dotted).then_some(classic);
    let keys = [sections.then_some(group), classic];
    let value = rows.find_map(|row| keys.iter().flatten().find_map(|k| row.strip_prefix(&k[..])));
    let commands = ["--list-cmds=builtins,main,others"];
    let expansion = value.filter(|v| !v.starts_with(b"!")).and_then(alias_words);
    if expansion.is_some() && listed(cwd, globals, &commands, name)? {
        return Ok(None);
    }
    Ok(expansion)
}
fn hooks(cwd: &Path, args: &[OsString], root: &Path) -> Result<Hooks> {
    let mut args = args.to_vec();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..16 {
        let Some(i) = subcommand(&args).filter(|i| seen.insert(args[*i].clone())) else {
            return Ok(Hooks::None); // Git handles invalid invocations and alias cycles.
        };
        let Some(words) = alias(cwd, &args[..i], args[i].as_bytes())? else {
            return rule(cwd, &args[..i], root, &args[i..]);
        };
        args.splice(i..=i, words);
    }
    anyhow::bail!("Git alias chain is too deep; run overgit base with the expanded command")
}
fn auto_attach(repo: &Repo) -> Result<bool> {
    let path = repo.local("detached.json");
    crate::repo::ensure_not_symlink(&path)?;
    let marker: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let complete = marker["complete"] != false;
    ensure!(complete, "incomplete detach; run overgit doctor --fix");
    Ok(marker["auto_attach"] == true)
}
fn changed_base_path(repo: &Repo) -> Result<Option<String>> {
    let manifest = repo.manifest()?;
    let staged = repo.base(["diff", "--cached", "--no-renames", "--name-only", "-z"])?;
    let mut staged_paths = staged.split(|b| *b == 0).flat_map(str::from_utf8);
    if let Some(path) = staged_paths.find(|path| manifest.manages_tracked(path)) {
        return Ok(Some(path.into()));
    }
    let managed = repo.managed_files(&manifest)?;
    let repo = repo.with_layout(managed.keys());
    for path in managed.keys() {
        if repo.work(path)? != repo.indexed(path, false)? {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
}
fn finish(root: &Path, notify: bool) -> Result<()> {
    // Rediscover after Git changes the index. Repo caches path spellings and gitlinks.
    let repo = Repo::discover(root)?;
    if let Some(operation) = repo.base_operation()? {
        if notify {
            eprintln!("overgit: overlay remains detached ({operation}).");
            eprintln!("Finish or abort with overgit base, or run overgit attach once Git is idle.");
        }
    } else if let Some(path) = changed_base_path(&repo)? {
        if notify {
            eprintln!("overgit: overlay remains detached because {path:?} has base changes.");
            eprintln!("Review with overgit base status, then commit or discard with overgit base.");
        }
    } else {
        repo.assert_no_sync()?;
        ops::reconcile(&repo, false, false)?;
    }
    Ok(())
}
pub fn run(cwd: &Path, args: &[OsString]) -> Result<i32> {
    crate::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut child = Command::new("git");
    child.current_dir(cwd).args(args);
    let passthrough = |git: &mut Command| Err(git.exec()).context("cannot start Git");
    let Some(i) = subcommand(args) else {
        return passthrough(&mut child);
    };
    let root = match git_path(cwd, &args[..i], "--show-toplevel")? {
        Some(root) if root.join(".overgit/.git").try_exists()? => root,
        _ => return passthrough(&mut child),
    };
    let hooks = hooks(cwd, args, &root)?;
    let writes = matches!(hooks, DetachAttach | IndexedPatch);
    if !writes && !root.join(".overgit/local/detached.json").try_exists()? {
        return passthrough(&mut child);
    }
    let repo = Repo::discover(&root)?;
    let same = git_path(cwd, &args[..i], "--absolute-git-dir")?.as_ref() == Some(&repo.git_dir);
    ensure!(same, "Git directory does not match the base worktree");
    child.env("OVERGIT_SKIP_HOOKS", "1");
    // Reads can run inside a hook. Older hooks must see the repository lock during writes.
    let mut git_status = || -> Result<i32> {
        let status = child.status().context("cannot start Git")?;
        Ok(status.code().unwrap_or(128 + status.signal().unwrap_or(0)))
    };
    let read = (hooks == ReadOnly).then(&mut git_status);
    let lock = repo.lock();
    if let Err(error) = &lock
        && matches!(error.downcast_ref(), Some(TryLockError::WouldBlock))
        && let Some(status) = read
    {
        return status;
    }
    let _lock = lock?;
    mutation::assert_clear(&repo)?;
    if writes && !repo.detached() {
        repo.assert_idle()?;
        ops::detach(&repo, false, true)?;
    }
    if hooks == IndexedPatch {
        // Replacing an owned file during detach invalidates Git's cached stat data.
        repo.base(["-c", TRACK_STAT, "update-index", "-q", "--refresh"])?;
    }
    let owned = repo.detached() && auto_attach(&repo)?;
    let result = read.unwrap_or_else(git_status);
    if owned && let Err(error) = finish(&repo.root, writes || hooks == Hooks::Commit) {
        let success = matches!(result, Ok(0));
        let outcome = if success { "succeeded" } else { "finished" };
        eprintln!("overgit: base Git command {outcome}, but re-attaching failed: {error:#}");
        eprintln!("overgit: overlay remains detached. Fix the problem and run overgit attach.");
        return result.map(|code| code.max(1));
    }
    result
}
