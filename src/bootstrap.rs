use crate::{
    git,
    model::{Kind, MANIFEST, Manifest},
    ops,
    repo::{Repo, atomic_write, ensure_not_symlink},
};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    ffi::OsString,
    fmt::Write as _,
    fs,
    os::unix::{ffi::OsStringExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
};

const HOOK_BEGIN: &str = "# BEGIN overgit managed hook";
const HOOK_END: &str = "# END overgit managed hook";

pub fn init(cwd: &Path, remote: Option<&str>, branch: &str) -> Result<i32> {
    let repo = Repo::discover(cwd)?;
    let _lock = repo.lock()?;
    repo.assert_idle()?;
    repo.base(["check-ref-format", "--branch", branch])?;
    let remote = remote.map(|remote| remote_url(cwd, remote)).transpose()?;
    ensure!(!repo.overlay_dir().exists(), "an overlay already exists");
    let directory = repo.root.join(".overgit");
    ensure_not_symlink(&directory)?;
    if directory.exists() {
        ensure!(
            fs::read_dir(&directory)?.next().is_none(),
            "nonempty .overgit directory; refusing to overwrite it"
        );
    } else {
        fs::create_dir(&directory)?;
    }
    git::run(
        &repo.root,
        [
            OsString::from("init"),
            OsString::from("--initial-branch"),
            OsString::from(branch),
            directory.into_os_string(),
        ],
    )?;
    configure(&repo)?;
    if let Some(remote) = remote {
        repo.overlay(["remote", "add", "origin", &remote])?;
        repo.overlay(["config", &format!("branch.{branch}.remote"), "origin"])?;
        repo.overlay([
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        ])?;
    }
    repo.save_manifest(&Manifest::default())?;
    repo.overlay(["commit", "-m", "overgit: initialise overlay"])?;
    ops::excludes(&repo)?;
    crate::say!("created overlay on branch {branch}");
    Ok(0)
}

pub fn configure(repo: &Repo) -> Result<()> {
    let root = repo
        .root
        .to_str()
        .context("non-UTF-8 repository roots are unsupported")?;
    repo.overlay(["config", "core.bare", "false"])?;
    repo.overlay(["config", "extensions.worktreeConfig", "true"])?;
    let worktree = if repo.overlay_dir().is_file() {
        root
    } else {
        "../.."
    };
    repo.overlay(["config", "--worktree", "core.worktree", worktree])?;
    // core.worktree is per checkout, never shared with linked overlay worktrees.
    let result = git::output(
        repo.overlay_cmd()
            .args(["config", "--local", "--unset-all", "core.worktree"]),
        None,
    )?;
    ensure!(
        result.status.success() || result.status.code() == Some(5),
        "cannot remove shared overlay core.worktree"
    );
    repo.overlay(["config", "status.showUntrackedFiles", "no"])?;
    let common = PathBuf::from(git::text(repo.overlay([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?)?);
    let exclude = common.join("info/exclude");
    ensure_not_symlink(&exclude)?;
    let original = match fs::read(&exclude) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut content = original.clone();
    for pattern in [b"/.git".as_slice(), b"/.overgit/.git/", b"/.overgit/local/"] {
        if !content
            .split(|byte| *byte == b'\n')
            .any(|line| line == pattern)
        {
            if !content.is_empty() && !content.ends_with(b"\n") {
                content.push(b'\n');
            }
            content.extend_from_slice(pattern);
            content.push(b'\n');
        }
    }
    if content != original {
        atomic_write(&exclude, &content)?;
    }
    Ok(())
}

fn remote_url(cwd: &Path, url: &str) -> Result<String> {
    ensure!(!url.is_empty(), "remote URL must not be empty");
    if url.contains(':') || Path::new(url).is_absolute() {
        return Ok(url.to_owned());
    }
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        cwd.canonicalize()?
    };
    Ok(cwd
        .join(url)
        .to_str()
        .context("non-UTF-8 remote path")?
        .to_owned())
}

pub fn clone_overlay(
    cwd: &Path,
    url: &str,
    dir: Option<&Path>,
    base: Option<&str>,
    branch: Option<&str>,
) -> Result<i32> {
    let url = remote_url(cwd, url)?;
    let target = dir.map_or_else(
        || cwd.to_path_buf(),
        |dir| {
            if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                cwd.join(dir)
            }
        },
    );
    if let Some(base) = base {
        let base = remote_url(cwd, base)?;
        if target.join(".git").exists() {
            let existing = Repo::discover(&target)?;
            let origin = git::text(existing.base(["remote", "get-url", "origin"])?)?;
            ensure!(origin == base, "existing base has a different origin");
        } else {
            git::run(
                cwd,
                [
                    OsString::from("clone"),
                    OsString::from("--"),
                    OsString::from(&base),
                    target.clone().into_os_string(),
                ],
            )?;
        }
    }
    let repo = Repo::discover(&target)?;
    let _lock = repo.lock()?;
    repo.assert_idle()?;
    repo.assert_no_sync()?;
    crate::mutation::assert_clear(&repo)?;
    ensure!(
        !repo.detached(),
        "overlay is detached; attach before cloning again"
    );
    if repo.overlay_dir().exists() {
        let origin = git::text(repo.overlay(["remote", "get-url", "origin"])?)?;
        ensure!(origin == url, "existing overlay has a different origin");
        if let Some(branch) = branch {
            let current = git::text(repo.overlay(["symbolic-ref", "--short", "HEAD"])?)?;
            ensure!(
                current == branch,
                "existing overlay is on branch {current}; requested {branch}"
            );
        }
        // An interrupted first clone may have an index but no materialized manifest.
        if !repo.root.join(MANIFEST).exists() {
            materialize_manifest(&repo)?;
        }
        ops::reconcile(&repo, false, false)?;
        crate::say!("overlay already present in {}", repo.root.display());
        return Ok(0);
    }
    let destination = repo.root.join(".overgit");
    ensure_not_symlink(&destination)?;
    if destination.exists() {
        ensure!(
            fs::read_dir(&destination)?.next().is_none(),
            "nonempty .overgit directory; refusing to overwrite it"
        );
    }
    let mut args = vec![OsString::from("clone"), OsString::from("--no-checkout")];
    if let Some(branch) = branch {
        args.extend([OsString::from("--branch"), OsString::from(branch)]);
    }
    args.extend([
        OsString::from("--"),
        OsString::from(&url),
        destination.into_os_string(),
    ]);
    git::run(&repo.root, args)?;
    configure(&repo)?;
    repo.overlay(["reset", "--mixed", "HEAD"])?;
    materialize_manifest(&repo)?;
    ops::reconcile(&repo, false, false)?;
    crate::say!("cloned overlay into {}", repo.root.display());
    Ok(0)
}

fn materialize_manifest(repo: &Repo) -> Result<()> {
    let snapshot = repo
        .indexed(MANIFEST, true)?
        .context("overlay has no manifest in its index")?;
    ensure!(
        snapshot.mode == "100644",
        "overlay manifest must be a regular file"
    );
    let manifest: Manifest = serde_json::from_slice(&snapshot.data)?;
    manifest.validate()?;
    let repo = &repo.with_layout(repo.managed_files(&manifest)?.keys());
    let index = repo.index(true)?;
    for path in index.keys() {
        if path == MANIFEST {
            continue;
        }
        let entry = manifest
            .entries
            .get(path)
            .with_context(|| format!("overlay tracks undeclared path {path:?}"))?;
        ensure!(
            entry.kind != Kind::Delete,
            "overlay tracks a whiteout at {path:?}"
        );
        repo.validate_leaf(path)?;
    }
    for (path, entry) in &manifest.entries {
        repo.validate_leaf(path)?;
        if entry.kind != Kind::Delete {
            ensure!(
                index.contains_key(path),
                "overlay content is missing for {path:?}"
            );
            if let Some(value) = repo.indexed(path, true)? {
                value.validate()?;
            }
        }
    }
    atomic_write(&repo.root.join(MANIFEST), &snapshot.data)?;
    Ok(())
}

pub fn hooks(repo: &Repo, install: bool) -> Result<i32> {
    let hook_dir = PathBuf::from(git::text(repo.base([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "hooks",
    ])?)?);
    ensure_not_symlink(&hook_dir)?;
    if install {
        fs::create_dir_all(&hook_dir)?;
    }
    let executable = std::env::current_exe()?;
    let executable = shell_quote(executable.to_str().context("non-UTF-8 executable path")?);
    for name in ["post-merge", "post-checkout", "post-rewrite"] {
        let path = hook_dir.join(name);
        ensure_not_symlink(&path)?;
        let mode =
            fs::metadata(&path).map_or(0o755, |metadata| metadata.permissions().mode() | 0o111);
        let original = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        if install && !shell_hook(&original) {
            eprintln!(
                "skipped non-shell hook {}; run overgit apply from it manually",
                path.display()
            );
            continue;
        }
        let mut content = strip_hook_block(&original)?;
        if install {
            let (shebang, body, generated, newline) = if content.starts_with("#!") {
                match content.find('\n') {
                    Some(end) => (&content[..=end], &content[end + 1..], false, false),
                    None => (content.as_str(), "", false, true),
                }
            } else {
                ("#!/bin/sh\n", content.as_str(), true, false)
            };
            let mut wrapped = shebang.to_owned();
            if newline {
                wrapped.push('\n');
            }
            writeln!(wrapped, "{HOOK_BEGIN}")?;
            if generated {
                writeln!(wrapped, "# overgit generated shebang")?;
            }
            if newline {
                writeln!(wrapped, "# overgit added shebang newline")?;
            }
            // Apply before user code so exit, exec and traps retain their normal
            // shell semantics. Keep stdin, variables and directory changes isolated.
            // A sentinel protects path newlines from command substitution trimming;
            // remove the sentinel and exactly one Git record terminator afterward.
            write!(
                wrapped,
                "(\n  if overgit_hook_root=$(git rev-parse --show-toplevel 2>/dev/null && printf .); then\n    overgit_hook_root=${{overgit_hook_root%.}}\n    overgit_hook_root=${{overgit_hook_root%?}}\n    if [ -n \"$overgit_hook_root\" ] && [ -e \"$overgit_hook_root/.overgit/.git\" ]; then\n      {executable} -C \"$overgit_hook_root\" apply >/dev/null\n    fi\n  fi\n) </dev/null || true\n{HOOK_END}\n"
            )?;
            wrapped.push_str(body);
            content = wrapped;
        }
        if content != original {
            atomic_write(&path, content.as_bytes())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
            crate::say!("{} {}", if install { "installed" } else { "removed" }, name);
        }
    }
    Ok(0)
}

fn shell_hook(content: &str) -> bool {
    let Some(first) = content.lines().next() else {
        return true;
    };
    if !first.starts_with("#!") {
        return true;
    }
    let interpreter = first.trim_start_matches("#!").trim();
    let words: Vec<_> = interpreter.split_whitespace().collect();
    let shell = if words.first().is_some_and(|word| {
        Path::new(word)
            .file_name()
            .is_some_and(|name| name == "env")
    }) {
        words.get(1).copied()
    } else {
        words.first().copied()
    };
    shell.is_some_and(|shell| {
        Path::new(shell).file_name().is_some_and(|name| {
            name == "sh" || name == "bash" || name == "dash" || name == "zsh" || name == "ksh"
        })
    })
}

fn strip_hook_block(content: &str) -> Result<String> {
    let mut result = String::new();
    let mut inside = false;
    let mut generated_shebang = false;
    let mut added_newline = false;
    for line in content.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == HOOK_BEGIN {
            ensure!(!inside, "nested managed hook block; repair manually");
            inside = true;
        } else if line.trim_end_matches(['\r', '\n']) == HOOK_END {
            ensure!(inside, "orphan managed hook end; repair manually");
            inside = false;
        } else if inside && line.trim_end() == "# overgit generated shebang" {
            generated_shebang = true;
        } else if inside && line.trim_end() == "# overgit added shebang newline" {
            added_newline = true;
        } else if !inside {
            result.push_str(line);
        }
    }
    ensure!(!inside, "unterminated managed hook block; repair manually");
    if generated_shebang {
        ensure!(
            result.starts_with("#!/bin/sh\n"),
            "managed hook shebang changed; repair manually"
        );
        result.drain(.."#!/bin/sh\n".len());
    }
    if added_newline
        && result
            .strip_suffix('\n')
            .is_some_and(|line| !line.contains('\n'))
    {
        result.pop();
    }
    Ok(result)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn worktree_add(
    repo: &Repo,
    path: &Path,
    commit: Option<&str>,
    branch: Option<&str>,
    overlay_branch: Option<&str>,
) -> Result<i32> {
    repo.assert_attached()?;
    repo.assert_no_sync()?;
    repo.overlay(["rev-parse", "--verify", "HEAD"])?;
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.cwd.join(path)
    };
    let mut args = vec![OsString::from("worktree"), OsString::from("add")];
    if let Some(branch) = branch {
        args.extend([OsString::from("-b"), OsString::from(branch)]);
    }
    args.extend([OsString::from("--"), target.clone().into_os_string()]);
    if let Some(commit) = commit {
        args.push(OsString::from(commit));
    }
    repo.base(args)?;
    let child = Repo::discover(&target)?;
    let selected_branch = if let Some(branch) = overlay_branch.or(branch) {
        branch.to_owned()
    } else {
        let output = git::output(
            child
                .base_cmd()
                .args(["symbolic-ref", "--quiet", "--short", "HEAD"]),
            None,
        )?;
        if output.status.success() {
            git::text(output.stdout)?
        } else if output.status.code() == Some(1) {
            child
                .root
                .file_name()
                .context("worktree has no directory name")?
                .to_str()
                .context("non-UTF-8 worktree name")?
                .to_owned()
        } else {
            git::check(&output)?;
            bail!("cannot determine worktree branch");
        }
    };
    configure(repo)?;
    let exists = git::output(
        repo.overlay_cmd().args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{selected_branch}"),
        ]),
        None,
    )?;
    ensure!(
        exists.status.success() || exists.status.code() == Some(1),
        "cannot inspect overlay branch"
    );
    let mut args = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--no-checkout"),
    ];
    if !exists.status.success() {
        args.extend([OsString::from("-b"), OsString::from(&selected_branch)]);
    }
    args.extend([
        OsString::from("--"),
        child.root.join(".overgit").into_os_string(),
        OsString::from(if exists.status.success() {
            &selected_branch
        } else {
            "HEAD"
        }),
    ]);
    repo.overlay(args).with_context(|| format!("base worktree {} exists, but linking its overlay failed; remove that worktree or retry with a different --overlay-branch", child.root.display()))?;
    configure(&child)?;
    child.overlay(["reset", "--mixed", "HEAD"])?;
    materialize_manifest(&child)?;
    ops::reconcile(&child, false, false)?;
    crate::say!(
        "created worktree {} with overlay branch {selected_branch}",
        child.root.display()
    );
    Ok(0)
}

fn worktree_roots(repo: &Repo) -> Result<Vec<PathBuf>> {
    let bytes = repo.base(["worktree", "list", "--porcelain", "-z"])?;
    let mut roots = Vec::new();
    for record in bytes.split(|byte| *byte == 0) {
        if let Some(path) = record.strip_prefix(b"worktree ") {
            roots.push(PathBuf::from(OsString::from_vec(path.to_vec())));
        }
    }
    Ok(roots)
}

pub fn worktree_list(repo: &Repo) -> Result<i32> {
    for root in worktree_roots(repo)? {
        if !root.exists() {
            continue;
        }
        let child = match Repo::discover(&root) {
            Ok(child) => child,
            Err(error) => {
                crate::say!("{}\tunavailable\t{}", root.display(), error);
                continue;
            }
        };
        let branch =
            symbolic_branch(&child, false).unwrap_or_else(|error| format!("unavailable ({error})"));
        let overlay = if child.overlay_dir().exists() {
            let overlay_branch = symbolic_branch(&child, true)
                .unwrap_or_else(|error| format!("unavailable ({error})"));
            let owned = child.manifest().map_or_else(
                |_| "invalid manifest".into(),
                |manifest| format!("{} paths", manifest.entries.len()),
            );
            format!(
                "overlay {overlay_branch}, {owned}{}",
                if child.detached() { ", detached" } else { "" }
            )
        } else {
            "no overlay".into()
        };
        crate::say!(
            "{}\t{branch}\t{overlay}{}",
            root.display(),
            if root == repo.root { "\tcurrent" } else { "" }
        );
    }
    Ok(0)
}

fn symbolic_branch(repo: &Repo, overlay: bool) -> Result<String> {
    let mut command = if overlay {
        repo.overlay_cmd()
    } else {
        repo.base_cmd()
    };
    let output = git::output(
        command.args(["symbolic-ref", "--quiet", "--short", "HEAD"]),
        None,
    )?;
    if output.status.success() {
        git::text(output.stdout)
    } else if output.status.code() == Some(1) {
        Ok("(detached HEAD)".into())
    } else {
        git::check(&output)?;
        bail!("cannot inspect branch")
    }
}

pub fn worktree_remove(repo: &Repo, path: &Path, force: bool) -> Result<i32> {
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.cwd.join(path)
    }
    .canonicalize()?;
    ensure!(target != repo.root, "cannot remove the current worktree");
    let roots = worktree_roots(repo)?;
    ensure!(
        roots
            .iter()
            .skip(1)
            .any(|root| root.canonicalize().is_ok_and(|root| root == target)),
        "target is not a linked worktree of this base"
    );
    let child = Repo::discover(&target)?;
    if !force {
        crate::mutation::assert_clear(&child)?;
        let pull = child.local("pull");
        ensure_not_symlink(&pull)?;
        ensure!(
            !pull.try_exists()?,
            "target has a pending pull; complete it or preserve its isolated work before removal"
        );
        let backups = child.local("backups");
        ensure_not_symlink(&backups)?;
        ensure!(
            !backups.try_exists()? || fs::read_dir(&backups)?.next().transpose()?.is_none(),
            "target has recovery backups; move them outside the worktree before removal or use --force to discard them"
        );
    }
    if child.overlay_dir().exists() {
        let common = PathBuf::from(git::text(child.overlay([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?)?);
        let caller_common = PathBuf::from(git::text(repo.overlay([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?)?);
        ensure!(
            common == caller_common,
            "target has an independent overlay; refusing to delete its repository"
        );
        if !force {
            child.assert_no_sync()?;
            let dirty = if child.detached() {
                child.overlay(["diff", "--cached", "--name-only", "-z"])?
            } else {
                child.overlay(["status", "--porcelain=v1", "-z", "--untracked-files=no"])?
            };
            ensure!(
                dirty.is_empty(),
                "target overlay has uncommitted changes; commit them or use --force"
            );
        }
    }
    let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        args.push(OsString::from("--force"));
    }
    args.extend([OsString::from("--"), target.clone().into_os_string()]);
    repo.base(args)?;
    repo.overlay(["worktree", "prune"])?;
    ops::excludes(repo)?;
    crate::say!(
        "removed worktree {}; branches were retained",
        target.display()
    );
    Ok(0)
}
