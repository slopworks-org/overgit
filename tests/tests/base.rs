use super::{Sandbox, cli, cli_code, content, git, git_ok, isolated, manifest, write};
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
};

fn own(sandbox: &Sandbox) {
    sandbox.init();
    write(&sandbox.root, "owned", b"overlay\n");
    sandbox.run(&["add", "owned"]);
    sandbox.run(&["commit", "-m", "overlay"]);
}

fn upstream(sandbox: &Sandbox) -> PathBuf {
    let writer = sandbox.temporary.path().join("upstream");
    git_ok(&sandbox.root, &["clone", ".", writer.to_str().unwrap()]);
    git_ok(
        &sandbox.root,
        &["remote", "add", "origin", writer.to_str().unwrap()],
    );
    git_ok(&sandbox.root, &["fetch", "origin"]);
    git_ok(&sandbox.root, &["branch", "--set-upstream-to=origin/main"]);
    writer
}

fn commit(root: &Path, message: &str) {
    git_ok(root, &["add", "--all"]);
    git_ok(root, &["commit", "-m", message]);
}

fn marker(sandbox: &Sandbox) -> PathBuf {
    sandbox.root.join(".overgit/local/detached.json")
}

fn assert_passthrough(root: &Path, args: &[&str]) {
    let expected = git(root, args);
    let mut base = vec!["base"];
    if args == ["--help"] {
        base.push("--");
    }
    base.extend(args);
    let actual = cli(root, &base);
    assert_eq!(actual.status.code(), expected.status.code(), "{base:?}");
    assert_eq!(actual.stdout, expected.stdout, "{base:?}");
    assert_eq!(actual.stderr, expected.stderr, "{base:?}");
}

#[test]
fn mv_and_rm_keep_base_changes_staged_and_save_unsaved_overlay_bytes() {
    for (command, args) in [
        ("mv", vec!["base", "mv", "owned", "renamed"]),
        ("rm", vec!["base", "rm", "--sparse", "-f", "owned"]),
    ] {
        let sandbox = Sandbox::new(&[("owned", b"base\n"), ("other", b"base sibling\n")]);
        own(&sandbox);
        let root = &sandbox.root;
        write(root, "owned", b"unsaved private bytes\n");
        let out = sandbox.run(&args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("remains detached") && stderr.contains("owned"),
            "{stderr}"
        );
        let expected_stdout: &[u8] = if command == "rm" {
            b"rm 'owned'\n"
        } else {
            b""
        };
        assert_eq!(out.stdout, expected_stdout);
        assert!(marker(&sandbox).exists());
        assert!(!root.join("owned").exists());
        assert_eq!(
            sandbox.overlay(&["show", ":owned"]),
            b"unsaved private bytes\n"
        );
        let mut staged = b"D\towned\n".to_vec();
        if command == "mv" {
            staged.extend(b"A\trenamed\n");
            assert_eq!(git_ok(root, &["show", ":renamed"]), b"base\n");
            assert_eq!(content(root, "renamed"), b"base\n");
            assert_eq!(git_ok(root, &["ls-files", "-v", "renamed"]), b"H renamed\n");
        }
        assert_eq!(
            git_ok(root, &["diff", "--cached", "--no-renames", "--name-status"]),
            staged
        );
        let status = sandbox.run(&["base", "status", "--porcelain"]);
        assert_eq!(status.stdout, git_ok(root, &["status", "--porcelain"]));
        assert!(status.stderr.is_empty());
        assert!(marker(&sandbox).exists());

        sandbox.run(&["base", "commit", "-m", "base change"]);
        assert!(!marker(&sandbox).exists());
        assert_eq!(content(root, "owned"), b"unsaved private bytes\n");
        assert!(git_ok(root, &["ls-files", "owned"]).is_empty());
        assert!(
            git_ok(root, &["ls-files", "-v"])
                .split(|&b| b == b'\n')
                .all(|row| row.is_empty() || row.starts_with(b"H "))
        );
        if command == "mv" {
            assert_eq!(git_ok(root, &["show", "HEAD:renamed"]), b"base\n");
            write(root, "renamed", b"visible base edit\n");
            assert_eq!(
                git_ok(root, &["status", "--short", "--", "renamed"]),
                b" M renamed\n"
            );
        }
    }
}

const BASE_PATCH: &[u8] =
    b"diff --git a/owned b/owned\n--- a/owned\n+++ b/owned\n@@ -1 +1 @@\n-base\n+patched\n";

#[test]
fn apply_protects_owned_paths_when_writing_the_worktree_or_index() {
    for options in [
        &[][..],
        &["--check", "--apply", "--index"],
        &["--stat", "--no-stat"],
        &["--cached"],
    ] {
        let sandbox = Sandbox::new(&[("owned", b"base\n")]);
        own(&sandbox);
        let root = &sandbox.root;
        write(root, "owned", b"unsaved overlay edit\n");
        write(root, "patch", BASE_PATCH);
        let mut args = vec!["base", "apply"];
        args.extend(options);
        args.push("patch");
        let out = sandbox.run(&args);
        assert!(String::from_utf8_lossy(&out.stderr).contains("remains detached"));
        assert!(marker(&sandbox).exists());
        if options.contains(&"--cached") {
            assert_eq!(content(root, "owned"), b"base\n");
            assert_eq!(git_ok(root, &["show", ":owned"]), b"patched\n");
        } else {
            assert_eq!(content(root, "owned"), b"patched\n");
        }
        assert_eq!(
            sandbox.overlay(&["show", ":owned"]),
            b"unsaved overlay edit\n"
        );
        assert_eq!(git_ok(root, &["ls-files", "-v", "owned"]), b"H owned\n");
        if options.contains(&"--cached") {
            let commit = sandbox.run(&[
                "base",
                "-c",
                "alias.record=commit",
                "record",
                "-m",
                "patched base",
            ]);
            assert!(String::from_utf8_lossy(&commit.stderr).contains("overlay remains detached"));
            assert!(marker(&sandbox).exists());
            sandbox.run(&["base", "restore", "--worktree", "owned"]);
        } else {
            sandbox.run(&["base", "commit", "-am", "patched base"]);
        }
        assert_eq!(git_ok(root, &["show", "HEAD:owned"]), b"patched\n");
        assert_eq!(content(root, "owned"), b"unsaved overlay edit\n");
        assert!(!marker(&sandbox).exists());
    }
}

#[test]
fn mv_rm_and_apply_previews_pass_through_during_a_base_operation() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    write(root, "owned", b"unsaved overlay edit\n");
    write(root, "patch", BASE_PATCH);
    write(
        root,
        ".git/MERGE_HEAD",
        &git_ok(root, &["rev-parse", "HEAD"]),
    );
    git_ok(root, &["config", "alias.preview-rm", "rm --sparse -n"]);
    for args in [
        &["mv", "-h"][..],
        &["rm", "-h"],
        &["apply", "-h"],
        &["mv", "-vfn", "owned", "renamed"],
        &["mv", "--dry-run", "owned", "renamed"],
        &["rm", "--sparse", "-fn", "owned"],
        &["rm", "--dry-run", "owned"],
        &["preview-rm", "owned"],
        &["apply", "--check", "patch"],
        &["apply", "--stat", "--summary", "patch"],
        &["apply", "--numstat", "patch"],
    ] {
        assert_passthrough(root, args);
    }
    write(root, "-n", b"owned\n");
    for args in [
        &["base", "mv", "-n", "--no-dry-run", "owned", "renamed"][..],
        &["base", "rm", "--sparse", "--pathspec-from-file", "-n"],
        &["base", "rm", "-f", "--", "owned", "-n"],
        &["base", "apply", "--check", "--apply", "patch"],
        &["base", "apply", "--include", "--check", "patch"],
        &[
            "base",
            "apply",
            "--stat",
            "--build-fake-ancestor=owned",
            "patch",
        ],
    ] {
        let out = sandbox.reject(args);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("base operation in progress"),
            "{args:?}: {:?}",
            out.stderr
        );
    }
    assert!(!marker(&sandbox).exists());
    assert!(!root.join("renamed").exists());
    assert_eq!(content(root, "owned"), b"unsaved overlay edit\n");
    assert_eq!(sandbox.overlay(&["show", ":owned"]), b"overlay\n");
    assert!(git_ok(root, &["diff", "--cached", "--name-only"]).is_empty());
}

#[test]
fn destructive_clean_saves_unsaved_overlay_additions_and_preserves_metadata() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    let root = &sandbox.root;
    own(&sandbox);
    write(root, "private-addition", b"committed overlay contents\n");
    sandbox.run(&["add", "private-addition"]);
    sandbox.run(&["commit", "-m", "add private file"]);
    let before = manifest(root);
    write(root, ".overgit/local/keep", b"recovery data\n");
    write(root, "private-addition", b"unsaved private edit\n");
    write(root, "junk", b"disposable base output\n");
    let out = sandbox.run(&["base", "clean", "-fdx"]);
    assert_eq!(out.stdout, b"Removing junk\n");
    assert!(out.stderr.is_empty(), "{:?}", out.stderr);
    assert!(!root.join("junk").exists());
    assert!(!marker(&sandbox).exists());
    assert_eq!(content(root, "private-addition"), b"unsaved private edit\n");
    assert_eq!(
        sandbox.overlay(&["show", ":private-addition"]),
        b"unsaved private edit\n"
    );
    assert_eq!(manifest(root), before);
    assert_eq!(content(root, ".overgit/local/keep"), b"recovery data\n");

    write(root, "private-addition", b"another unsaved edit\n");
    write(root, "junk", b"still here\n");
    git_ok(root, &["config", "alias.purge", "clean -xffd"]);
    for args in [
        &["base", "clean", "-xffd"][..],
        &["base", "clean", "--force", "-f", "-x"],
        &[
            "base",
            "purge",
            "-e",
            "!/.overgit/",
            "--",
            ".overgit",
            "junk",
        ],
    ] {
        let out = sandbox.reject(args);
        let message = String::from_utf8_lossy(&out.stderr);
        for fragment in [
            "could delete .overgit",
            "single -f",
            "overgit base -c clean.requireForce=true clean -f",
        ] {
            assert!(message.contains(fragment), "{message}");
        }
        assert!(out.stdout.is_empty(), "{args:?}");
        assert_eq!(content(root, "junk"), b"still here\n");
        assert!(!marker(&sandbox).exists());
        assert_eq!(content(root, "private-addition"), b"another unsaved edit\n");
        assert_eq!(
            sandbox.overlay(&["show", ":private-addition"]),
            b"unsaved private edit\n"
        );
        assert_eq!(manifest(root), before);
        assert_eq!(content(root, ".overgit/local/keep"), b"recovery data\n");
    }
    git_ok(root, &["config", "clean.requireForce", "false"]);
    let retry = sandbox.run(&[
        "base",
        "-c",
        "clean.requireForce=true",
        "clean",
        "-f",
        "-dx",
        "--",
        "junk",
    ]);
    assert_eq!(retry.stdout, b"Removing junk\n");
    assert_eq!(content(root, ".overgit/local/keep"), b"recovery data\n");
    assert_eq!(content(root, "private-addition"), b"another unsaved edit\n");
}

#[test]
fn clean_checks_git_force_configuration_in_the_selected_worktree() {
    let sandbox = Sandbox::new(&[("owned", b"base\n"), ("subdir/tracked", b"base file\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    write(root, "owned", b"unsaved overlay edit\n");
    write(root, ".overgit/local/keep", b"recovery\n");
    write(root, "subdir/junk", b"junk\n");
    let opts = [
        "-C",
        "subdir",
        "--literal-pathspecs",
        "-c",
        "clean.requireForce=false",
    ];
    let metadata = root.join(".overgit");
    let mut probe = opts.to_vec();
    probe.extend(["clean", "-ndfx", "--", metadata.to_str().unwrap()]);
    let dangerous = !git_ok(root, &probe).is_empty();
    let mut args = vec!["base"];
    args.extend(opts);
    args.extend(["clean", "-dfx"]);
    let out = cli_code(root, &args, i32::from(dangerous));
    if dangerous {
        assert!(String::from_utf8_lossy(&out.stderr).contains("clean.requireForce=true"));
        assert!(out.stdout.is_empty());
    } else {
        assert_eq!(out.stdout, b"Removing junk\n");
    }
    assert_eq!(root.join("subdir/junk").exists(), dangerous);
    assert_eq!(content(root, ".overgit/local/keep"), b"recovery\n");
    assert_eq!(content(root, "owned"), b"unsaved overlay edit\n");
    assert!(!marker(&sandbox).exists());
}

#[test]
fn clean_of_ignored_files_protects_a_manually_detached_overlay() {
    let sandbox = Sandbox::new(&[("owned", b"base\n"), (".gitignore", b"*.tmp\n")]);
    let root = &sandbox.root;
    own(&sandbox);
    sandbox.run(&["detach"]);
    let before = fs::read(marker(&sandbox)).unwrap();
    write(root, ".overgit/local/recovery.tmp", b"keep\n");
    write(root, "remove.tmp", b"remove\n");
    write(root, "untracked", b"keep\n");
    let out = sandbox.run(&[
        "base",
        "clean",
        "-Xfd",
        "--",
        ".overgit/local",
        "remove.tmp",
        "untracked",
    ]);
    assert_eq!(out.stdout, b"Removing remove.tmp\n");
    assert!(!root.join("remove.tmp").exists());
    assert_eq!(content(root, "untracked"), b"keep\n");
    assert_eq!(content(root, ".overgit/local/recovery.tmp"), b"keep\n");
    assert_eq!(fs::read(marker(&sandbox)).unwrap(), before);
    sandbox.run(&["attach"]);
    assert_eq!(content(root, "owned"), b"overlay\n");
}

#[test]
fn clean_previews_pass_through_without_detaching_or_staging() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    write(root, "owned", b"unsaved overlay edit\n");
    write(root, "junk", b"leave alone\n");
    write(
        root,
        ".git/MERGE_HEAD",
        &git_ok(root, &["rev-parse", "HEAD"]),
    );
    git_ok(root, &["config", "alias.preview", "clean -nxffd"]);
    for args in [
        &["clean", "-h"][..],
        &["clean", "-xffdh"],
        &["clean", "-ff", "--help"],
        &["clean", "-nxffd"],
        &["clean", "-ffd", "--dry-run"],
        &["clean", "--exclude", "--", "-n", "-x"],
        &["preview"],
    ] {
        assert_passthrough(root, args);
    }
    for args in [
        &["base", "clean", "-dfx", "-e", "-n"][..],
        &["base", "clean", "-nfx", "--no-dry-run"],
        &["base", "clean", "-fx", "--", "-n"],
    ] {
        let out = sandbox.reject(args);
        assert!(String::from_utf8_lossy(&out.stderr).contains("base operation in progress"));
    }
    assert_eq!(content(root, "owned"), b"unsaved overlay edit\n");
    assert_eq!(content(root, "junk"), b"leave alone\n");
    assert!(!marker(&sandbox).exists());
}

#[test]
fn pull_preserves_all_ownership_and_prints_only_git_output() {
    let sandbox = Sandbox::new(&[
        ("owned", b"base\n"),
        ("hidden", b"base whiteout\n"),
        ("rules/old", b"base rules\n"),
    ]);
    let root = &sandbox.root;
    let writer = upstream(&sandbox);
    let control = sandbox.temporary.path().join("plain");
    git_ok(&writer, &["clone", ".", control.to_str().unwrap()]);
    own(&sandbox);
    sandbox.run(&["rm", "hidden", "rules"]);
    write(root, "rules/mine", b"private rules\n");
    write(root, "addition", b"private addition\n");
    sandbox.run(&["add", "rules/mine", "addition"]);
    sandbox.run(&["commit", "-m", "overlay layout"]);
    write(root, "owned", b"unsaved overlay edit\n");
    let before = manifest(root);

    write(&writer, "owned", b"upstream\n");
    write(&writer, "hidden", b"upstream whiteout\n");
    write(&writer, "rules/new", b"new rule\n");
    commit(&writer, "update base");
    let expected = git_ok(&control, &["pull"]);
    let output = sandbox.run(&["base", "pull"]);
    assert_eq!(output.stdout, expected);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("overgit:"));
    assert_eq!(
        git_ok(root, &["rev-parse", "HEAD"]),
        git_ok(&writer, &["rev-parse", "HEAD"])
    );
    assert_eq!(git_ok(root, &["show", "HEAD:owned"]), b"upstream\n");
    assert_eq!(content(root, "owned"), b"unsaved overlay edit\n");
    assert_eq!(content(root, "addition"), b"private addition\n");
    assert_eq!(content(root, "rules/mine"), b"private rules\n");
    for path in ["hidden", "rules/old", "rules/new"] {
        assert!(!root.join(path).exists(), "{path}");
        assert_eq!(
            git_ok(root, &["ls-files", "-v", "--", path]),
            format!("S {path}\n").as_bytes()
        );
    }
    assert!(!marker(&sandbox).exists());
    assert_eq!(manifest(root), before);
    cli_code(root, &["sync", "--dry-run"], 4);
    sandbox.clean_base();
}

#[test]
fn direct_commands_work_while_sync_is_pending() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    git_ok(root, &["config", "alias.st", "status"]);
    git_ok(root, &["config", "alias.ok", "!echo ok"]);
    let check_reads = || {
        for args in [
            &["status", "--porcelain=v1", "-z"][..],
            &["diff", "--", "owned"],
            &["log", "-1", "--format=%H"],
            &["rev-parse", "--show-toplevel"],
            &["st", "--short"],
            &["ok"],
        ] {
            assert_passthrough(root, args);
        }
        assert!(!marker(&sandbox).exists());
    };
    check_reads();
    sandbox.upstream(&[("owned", Some(b"upstream\n"))]);
    cli_code(root, &["sync"], 3);
    let pending = content(root, ".overgit/local/sync.json");
    let conflict = content(root, "owned");
    check_reads();
    for cmd in [
        "pull",
        "merge",
        "rebase",
        "checkout",
        "switch",
        "reset",
        "cherry-pick",
        "revert",
        "stash",
        "restore",
        "bisect",
        "am",
    ] {
        let out = cli_code(root, &["base", cmd, "--invalid-option"], 1);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("sync in progress"),
            "{cmd}"
        );
        assert!(out.stdout.is_empty());
    }
    git_ok(root, &["config", "man.viewer", "test-viewer"]);
    git_ok(root, &["config", "man.test-viewer.cmd", "echo"]);
    assert_passthrough(root, &["pull", "--help"]);
    assert_eq!(content(root, ".overgit/local/sync.json"), pending);
    assert_eq!(content(root, "owned"), conflict);
}

#[test]
fn forwards_outside_overlays_and_honors_git_help_and_version() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    for cwd in [&sandbox.root, sandbox.temporary.path()] {
        for args in [
            &["--help"][..],
            &["--version"],
            &["status", "--short"],
            &["rev-parse", "HEAD"],
            &["unknown-cmd"],
        ] {
            assert_passthrough(cwd, args);
        }
    }
    assert_passthrough(&sandbox.root, &["checkout", "main"]);
    git_ok(&sandbox.root, &["init", "-q", "disposable"]);
    let clean = sandbox.run(&["base", "clean", "-xffd"]);
    assert_eq!(clean.stdout, b"Removing disposable/\n");
    assert!(!sandbox.root.join("disposable").exists());
    assert!(!sandbox.root.join(".overgit").exists());
    for help in ["-h", "--help"] {
        let out = cli_code(sandbox.temporary.path(), &["base", help], 0);
        assert!(String::from_utf8_lossy(&out.stdout).contains("overgit base"));
    }
}

#[test]
fn aliases_and_ordered_global_options_pull_the_selected_worktree() {
    let first = Sandbox::new(&[("owned", b"first\n")]);
    let other = Sandbox::new(&[("owned", b"second\n")]);
    own(&first);
    let writer = upstream(&other);
    own(&other);
    let git_head = git_ok(&first.root, &["rev-parse", "HEAD"]);
    write(&first.root, ".git/MERGE_HEAD", &git_head);
    git_ok(&other.root, &["config", "alias.up", "pull"]);
    // A configured alias cannot shadow a real Git command.
    git_ok(&other.root, &["config", "alias.pull", "status"]);
    write(&writer, "owned", b"first upstream\n");
    commit(&writer, "first update");
    cli_code(
        first.temporary.path(),
        &[
            "-C",
            first.root.to_str().unwrap(),
            "base",
            "-C",
            other.temporary.path().to_str().unwrap(),
            "-C",
            "base",
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "alias.follow=-c 'alias.step=up --ff-only' step",
            "follow",
        ],
        0,
    );
    assert_eq!(
        git_ok(&other.root, &["show", "HEAD:owned"]),
        b"first upstream\n"
    );
    assert_eq!(content(&other.root, "owned"), b"overlay\n");
    write(&writer, "owned", b"second upstream\n");
    commit(&writer, "second update");
    cli_code(
        &first.root,
        &[
            "base",
            "--work-tree",
            ".",
            "--git-dir=.git",
            "-C",
            other.root.to_str().unwrap(),
            "--namespace",
            "",
            "up",
            "--ff-only",
        ],
        0,
    );
    assert_eq!(
        git_ok(&other.root, &["show", "HEAD:owned"]),
        b"second upstream\n"
    );
    assert_eq!(content(&other.root, "owned"), b"overlay\n");
    assert_eq!(git_ok(&first.root, &["rev-parse", "HEAD"]), git_head);
    assert_eq!(content(&first.root, "owned"), b"overlay\n");
    assert!(!marker(&first).exists());
    assert!(!marker(&other).exists());
}

#[test]
fn an_explicit_git_dir_protects_the_worktree_git_selects() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    write(root, "owned", b"unsaved private edit\n");
    let output = sandbox.run(&[
        "base",
        "--git-dir=.git",
        "checkout",
        "--ignore-skip-worktree-bits",
        "HEAD",
        "--",
        "owned",
    ]);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(content(root, "owned"), b"unsaved private edit\n");
    assert_eq!(
        sandbox.overlay(&["show", ":owned"]),
        b"unsaved private edit\n"
    );
    assert!(!marker(&sandbox).exists());
    sandbox.clean_base();
}

#[test]
fn bare_and_read_only_git_dir_invocations_pass_through() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    write(
        root,
        ".git/MERGE_HEAD",
        &git_ok(root, &["rev-parse", "HEAD"]),
    );
    assert_passthrough(root, &["--git-dir", ".git", "stash", "list"]);
    assert_passthrough(root, &["--bare", "--git-dir=.git", "stash", "list"]);
    let bare = sandbox.temporary.path().join("bare");
    git_ok(root, &["clone", "--bare", ".", bare.to_str().unwrap()]);
    assert_passthrough(&bare, &["checkout", "main"]);
    assert_passthrough(&bare, &["--git-dir=.", "switch", "main"]);
    assert_eq!(content(root, "owned"), b"overlay\n");
    assert!(!marker(&sandbox).exists());
}

fn diverge(sandbox: &Sandbox) {
    let root = &sandbox.root;
    git_ok(root, &["checkout", "-b", "topic"]);
    write(root, "owned", b"topic\n");
    commit(root, "topic edit");
    git_ok(root, &["checkout", "main"]);
    write(root, "owned", b"main\n");
    commit(root, "main edit");
    own(sandbox);
}

#[test]
fn merge_conflict_stays_detached_until_base_commit() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    diverge(&sandbox);
    let root = &sandbox.root;
    let out = cli_code(root, &["base", "merge", "topic"], 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("overlay remains detached") && stderr.contains("overgit attach"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("detached"));
    let marker_before = fs::read(marker(&sandbox)).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&marker_before).unwrap();
    assert_eq!(state["auto_attach"], true);
    for args in [
        &["status", "--porcelain"][..],
        &["rev-parse", "--short", "HEAD"],
        &["diff", "--stat"],
        &["-c", "alias.st=status --short", "st"],
        &["rev-parse", "--verify", "refs/heads/absent"],
    ] {
        assert_passthrough(root, args);
    }
    assert_eq!(fs::read(marker(&sandbox)).unwrap(), marker_before);
    write(root, "owned", b"resolved\n");
    let add = sandbox.run(&["base", "add", "owned"]);
    assert!(add.stderr.is_empty());
    assert_eq!(fs::read(marker(&sandbox)).unwrap(), marker_before);
    assert_eq!(content(root, "owned"), b"resolved\n");
    sandbox.run(&["base", "commit", "-m", "resolve merge"]);
    assert!(!root.join(".git/MERGE_HEAD").exists());
    assert_eq!(git_ok(root, &["show", "HEAD:owned"]), b"resolved\n");
    assert_eq!(content(root, "owned"), b"overlay\n");
    assert!(!marker(&sandbox).exists());
    sandbox.clean_base();
}

#[test]
fn rebase_abort_and_continue_reattach_with_installed_hooks() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    diverge(&sandbox);
    let root = &sandbox.root;
    sandbox.run(&["hooks", "install"]);
    for action in ["--abort", "--continue"] {
        let start = cli_code(root, &["base", "rebase", "topic"], 1);
        assert!(String::from_utf8_lossy(&start.stderr).contains("overlay remains detached"));
        assert!(marker(&sandbox).exists());
        if action == "--continue" {
            write(root, "owned", b"rebased\n");
            sandbox.run(&["base", "add", "owned"]);
        }
        sandbox.run(&["base", "rebase", action]);
        assert!(!marker(&sandbox).exists());
        assert_eq!(content(root, "owned"), b"overlay\n");
    }
    assert_eq!(git_ok(root, &["show", "HEAD:owned"]), b"rebased\n");
    sandbox.clean_base();
}

#[test]
fn a_manual_detach_survives_a_base_pull_and_hooks() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    let writer = upstream(&sandbox);
    own(&sandbox);
    sandbox.run(&["hooks", "install"]);
    sandbox.run(&["detach"]);
    // Manual markers written by previous releases have no ownership field.
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(marker(&sandbox)).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("auto_attach");
    let original = serde_json::to_vec(&legacy).unwrap();
    fs::write(marker(&sandbox), &original).unwrap();
    write(&writer, "owned", b"upstream\n");
    commit(&writer, "change owned");
    sandbox.run(&["base", "pull"]);
    sandbox.run(&["base", "status"]);
    assert_eq!(content(&sandbox.root, "owned"), b"upstream\n");
    assert_eq!(fs::read(marker(&sandbox)).unwrap(), original);
    sandbox.run(&["attach"]);
    assert_eq!(content(&sandbox.root, "owned"), b"overlay\n");
}

#[test]
fn failed_git_commands_keep_their_exit_code_and_restore_the_overlay() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    for args in [["checkout", "nonexistent"], ["reset", "--unknown-option"]] {
        assert_passthrough(&sandbox.root, &args);
        assert!(!marker(&sandbox).exists());
        assert_eq!(content(&sandbox.root, "owned"), b"overlay\n");
    }
}

#[test]
fn help_and_stash_reads_pass_through_during_a_base_merge_including_aliases() {
    let sandbox = Sandbox::new(&[("owned", b"base\n"), ("other", b"original\n")]);
    let root = &sandbox.root;
    write(root, "other", b"stashed\n");
    git_ok(root, &["stash", "push"]);
    git_ok(root, &["checkout", "-b", "topic"]);
    write(root, "other", b"topic\n");
    commit(root, "topic");
    git_ok(root, &["checkout", "main"]);
    own(&sandbox);
    git_ok(root, &["merge", "--no-ff", "--no-commit", "topic"]);
    assert!(root.join(".git/MERGE_HEAD").exists());
    for (name, value) in [
        ("alias.co", "checkout"),
        ("alias.usage", "co -h"),
        ("alias.s", "stash"),
        ("alias.sl", "-c advice.statusHints=false s list"),
        ("alias.ss", "s show"),
    ] {
        git_ok(root, &["config", name, value]);
    }
    for args in [
        &["checkout", "-h"][..],
        &["co", "-h"],
        &["usage"],
        &["stash", "list"],
        &["stash", "show"],
        &["sl"],
        &["ss", "--stat"],
    ] {
        assert_passthrough(root, args);
        assert!(!marker(&sandbox).exists());
        assert_eq!(content(root, "owned"), b"overlay\n");
    }
    let out = sandbox.reject(&["base", "stash", "pop"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("base operation in progress"));
}

#[test]
fn a_user_hook_can_run_base_status_without_locking_or_remounting() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    write(
        &sandbox.root,
        ".git/hooks/post-checkout",
        br#"#!/bin/sh
set -eu
test "$OVERGIT_SKIP_HOOKS" = 1
test -f .overgit/local/detached.json
test "$(cat owned)" = base
(unset GIT_DIR GIT_WORK_TREE OVERGIT_SKIP_HOOKS; "$TEST_OVERGIT" base status --porcelain)
test -f .overgit/local/detached.json
echo 'hook ran'
"#,
    );
    sandbox.run(&["hooks", "install"]);
    let binary = env!("CARGO_BIN_EXE_overgit");
    let output = isolated(Command::new(binary).current_dir(&sandbox.root))
        .env("TEST_OVERGIT", binary)
        .args(["base", "checkout", "-b", "next"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Git redirects post-checkout hook stdout to stderr on some versions.
    let combined = [output.stdout, output.stderr].concat();
    assert!(String::from_utf8_lossy(&combined).contains("hook ran"));
    assert_eq!(content(&sandbox.root, "owned"), b"overlay\n");
    assert!(!marker(&sandbox).exists());
}

#[test]
fn successful_git_with_an_obstructed_attach_leaves_recoverable_state() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    let root = &sandbox.root;
    git_ok(root, &["checkout", "-b", "directory"]);
    fs::remove_file(root.join("owned")).unwrap();
    write(root, "owned/child", b"upstream child\n");
    commit(root, "replace file with directory");
    git_ok(root, &["checkout", "main"]);
    own(&sandbox);
    let output = cli_code(root, &["base", "switch", "directory"], 1);
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    for message in [
        "succeeded",
        "re-attaching failed",
        "remains detached",
        "overgit attach",
    ] {
        assert!(stderr.contains(message), "{stderr}");
    }
    assert_eq!(git_ok(root, &["branch", "--show-current"]), b"directory\n");
    assert_eq!(content(root, "owned/child"), b"upstream child\n");
    assert_eq!(sandbox.overlay(&["show", ":owned"]), b"overlay\n");
    assert!(marker(&sandbox).exists());
    let bad = ["reset", "--unknown-option"];
    let code = git(root, &bad).status.code().unwrap();
    let both_failed = cli_code(root, &["base", bad[0], bad[1]], code);
    assert!(String::from_utf8_lossy(&both_failed.stderr).contains("re-attaching failed"));
    assert_eq!(content(root, "owned/child"), b"upstream child\n");
    sandbox.run(&["base", "switch", "main"]);
    assert_eq!(content(root, "owned"), b"overlay\n");
    assert!(!marker(&sandbox).exists());
}

#[test]
fn unmerged_stash_paths_defer_attach_without_a_merge_head() {
    let sandbox = Sandbox::new(&[("owned", b"original\n")]);
    let root = &sandbox.root;
    write(root, "owned", b"stashed\n");
    git_ok(root, &["stash", "push", "-m", "pending edit"]);
    write(root, "owned", b"base changed\n");
    commit(root, "change base");
    own(&sandbox);

    let conflict = cli_code(root, &["base", "stash", "pop"], 1);
    assert!(!root.join(".git/MERGE_HEAD").exists());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("unmerged index"));
    assert!(marker(&sandbox).exists());
    sandbox.run(&["base", "reset", "--hard", "HEAD"]);
    assert!(!marker(&sandbox).exists());
    assert_eq!(content(root, "owned"), b"overlay\n");
}

#[test]
fn popping_a_stash_leaves_owned_base_edits_visible_until_committed() {
    for staged in [false, true] {
        let sandbox = Sandbox::new(&[("owned", b"original\n")]);
        let root = &sandbox.root;
        write(root, "owned", b"base stash contents\n");
        if staged {
            git_ok(root, &["add", "owned"]);
        }
        git_ok(root, &["stash", "push"]);
        own(&sandbox);
        let mut pop = vec!["base", "stash", "pop"];
        if staged {
            pop.push("--index");
        }
        let out = sandbox.run(&pop);
        let notice = String::from_utf8_lossy(&out.stderr);
        for word in ["remains detached", "owned", "commit", "discard"] {
            assert!(notice.contains(word), "{notice}");
        }
        assert!(!String::from_utf8_lossy(&out.stdout).contains("overgit:"));
        assert!(git_ok(root, &["stash", "list"]).is_empty());
        assert_eq!(content(root, "owned"), b"base stash contents\n");
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(marker(&sandbox)).unwrap()).unwrap();
        assert_eq!(state["auto_attach"], true);
        let status = sandbox.run(&["base", "status", "--porcelain"]);
        assert!(status.stderr.is_empty());
        assert_eq!(
            status.stdout,
            if staged { b"M  owned\n" } else { b" M owned\n" }
        );
        assert_passthrough(root, &["rev-parse", "--short", "HEAD"]);
        assert_passthrough(root, &["branch", "--show-current"]);
        assert_eq!(content(root, "owned"), b"base stash contents\n");
        sandbox.run(&["base", "commit", "-am", "keep stashed base change"]);
        assert_eq!(
            git_ok(root, &["show", "HEAD:owned"]),
            b"base stash contents\n"
        );
        assert_eq!(content(root, "owned"), b"overlay\n");
        assert!(!marker(&sandbox).exists());
        sandbox.clean_base();
    }
}

#[test]
fn unrelated_unstaged_base_edits_survive_reattachment() {
    let sandbox = Sandbox::new(&[("owned", b"base\n"), ("other", b"unchanged\n")]);
    let root = &sandbox.root;
    write(root, "other", b"stashed unrelated edit\n");
    git_ok(root, &["stash", "push"]);
    own(&sandbox);
    sandbox.run(&["base", "stash", "pop"]);
    assert_eq!(git_ok(root, &["status", "--porcelain"]), b" M other\n");
    assert_eq!(content(root, "owned"), b"overlay\n");
    assert_eq!(content(root, "other"), b"stashed unrelated edit\n");
    assert!(!marker(&sandbox).exists());
}

#[test]
fn an_untracked_file_restored_at_an_owned_addition_survives_stash_pop() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    let root = &sandbox.root;
    write(root, "addition", b"untracked base contents\n");
    git_ok(root, &["stash", "push", "--include-untracked"]);
    own(&sandbox);
    write(root, "addition", b"private addition\n");
    sandbox.run(&["add", "addition"]);
    sandbox.run(&["commit", "-m", "own the addition"]);
    let out = sandbox.run(&["base", "stash", "pop"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("addition"));
    assert_eq!(content(root, "addition"), b"untracked base contents\n");
    assert!(marker(&sandbox).exists());
    fs::remove_file(root.join("addition")).unwrap();
    assert_passthrough(root, &["status", "--short"]);
    assert_eq!(content(root, "addition"), b"private addition\n");
    assert!(!marker(&sandbox).exists());
}

#[test]
fn add_silently_reattaches_after_restaging_the_original_base_contents() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    sandbox.run(&["base", "rm", "owned"]);
    assert!(marker(&sandbox).exists());
    write(&sandbox.root, "owned", b"base\n");
    let add = sandbox.run(&["base", "add", "owned"]);
    assert!(add.stdout.is_empty() && add.stderr.is_empty());
    assert!(!marker(&sandbox).exists());
    assert_eq!(content(&sandbox.root, "owned"), b"overlay\n");
    sandbox.clean_base();
}

#[test]
fn a_staged_base_deletion_under_an_opaque_directory_defers_reattachment() {
    let sandbox = Sandbox::new(&[("owned", b"base\n"), ("rules/child", b"rule\n")]);
    let root = &sandbox.root;
    git_ok(root, &["rm", "rules/child"]);
    git_ok(root, &["stash", "push"]);
    own(&sandbox);
    sandbox.run(&["rm", "rules"]);
    sandbox.run(&["commit", "-m", "mask rules"]);
    let out = sandbox.run(&["base", "stash", "pop", "--index"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("rules/child"));
    assert_eq!(
        git_ok(root, &["status", "--porcelain"]),
        b"D  rules/child\n"
    );
    assert!(marker(&sandbox).exists());
    sandbox.run(&[
        "base",
        "restore",
        "--source=HEAD",
        "--staged",
        "--worktree",
        "rules/child",
    ]);
    assert!(!root.join("rules/child").exists());
    assert!(!marker(&sandbox).exists());
    sandbox.clean_base();
}

#[test]
fn shell_aliases_external_commands_and_alias_cycles_keep_git_behavior() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    git_ok(root, &["config", "alias.ok", "!echo ok"]);
    assert_eq!(sandbox.run(&["base", "ok"]).stdout, b"ok\n");
    git_ok(root, &["config", "alias.loop", "again"]);
    git_ok(root, &["config", "alias.again", "loop"]);
    assert_passthrough(root, &["loop"]);

    let bin = sandbox.temporary.path().join("bin");
    write(
        &bin,
        "git-extra-test",
        b"#!/bin/sh\ntest ! -e .overgit/local/detached.json && cat owned\n",
    );
    fs::set_permissions(
        bin.join("git-extra-test"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git_ok(root, &["config", "alias.extra-test", "reset --hard"]);
    let old_path = std::env::var_os("PATH").unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&old_path))).unwrap();
    let out = isolated(Command::new(env!("CARGO_BIN_EXE_overgit")).current_dir(root))
        .env("PATH", path)
        .args(["base", "extra-test"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"overlay\n");
    if git_ok(root, &["help", "--config"])
        .split(|b| *b == b'\n')
        .any(|key| key == b"alias.*.command")
    {
        git_ok(root, &["config", "alias.grouped", "status"]);
        git_ok(root, &["config", "alias.grouped.command", "stash push"]);
        write(
            root,
            ".git/MERGE_HEAD",
            &git_ok(root, &["rev-parse", "HEAD"]),
        );
        let rejected = sandbox.reject(&["base", "grouped"]);
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("base operation in progress"));
    }
}

#[test]
fn legacy_apply_cannot_acquire_the_repository_lock_during_a_base_write() {
    let sandbox = Sandbox::new(&[("owned", b"base\n")]);
    own(&sandbox);
    let root = &sandbox.root;
    // Previous installers call apply unconditionally and ignore its exit code.
    write(
        root,
        ".git/hooks/post-checkout",
        br#"#!/bin/sh
set -eu
unset OVERGIT_SKIP_HOOKS
if "$TEST_OVERGIT" apply; then exit 9; fi
"$TEST_OVERGIT" list >/dev/null
test -f .overgit/local/detached.json
test "$(cat owned)" = base
"#,
    );
    fs::set_permissions(
        root.join(".git/hooks/post-checkout"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_overgit");
    let run = |branch| {
        isolated(Command::new(binary).current_dir(root))
            .env("TEST_OVERGIT", binary)
            .args(["base", "checkout", "-b", branch])
            .output()
            .unwrap()
    };
    let automatic = run("auto");
    assert!(
        automatic.status.success(),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .contains("another overgit command holds the repository lock")
    );
    assert_eq!(content(root, "owned"), b"overlay\n");
    sandbox.run(&["detach"]);
    let manual = run("manual");
    assert!(
        manual.status.success(),
        "{}",
        String::from_utf8_lossy(&manual.stderr)
    );
    assert_eq!(content(root, "owned"), b"base\n");
    assert!(marker(&sandbox).exists());
    assert!(
        String::from_utf8_lossy(&manual.stderr)
            .contains("another overgit command holds the repository lock")
    );
}
