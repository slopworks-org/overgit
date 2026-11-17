#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write as _,
        os::unix::{fs::PermissionsExt as _, process::ExitStatusExt as _},
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
    };
    use tempfile::TempDir;

    struct Sandbox {
        temporary: TempDir,
        root: PathBuf,
    }
    impl Sandbox {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("base");
            fs::create_dir(&root).unwrap();
            let sandbox = Self { temporary, root };
            sandbox.git(&["init", "--initial-branch=main"]);
            fs::write(sandbox.root.join("base"), b"base\n").unwrap();
            sandbox.git(&["add", "--all"]);
            sandbox.git(&["commit", "-m", "base"]);
            sandbox.cli(&["init"], 0);
            sandbox
        }
        fn command(&self, executable: &str) -> Command {
            let mut command = Command::new(executable);
            command
                .current_dir(&self.root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", self.temporary.path())
                .env("TMPDIR", self.temporary.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Mutation Test")
                .env("GIT_AUTHOR_EMAIL", "mutation@example.invalid")
                .env("GIT_COMMITTER_NAME", "Mutation Test")
                .env("GIT_COMMITTER_EMAIL", "mutation@example.invalid")
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "protocol.allow")
                .env("GIT_CONFIG_VALUE_0", "never");
            command
        }
        fn checked(&self, executable: &str, arguments: &[&str], code: i32) -> Output {
            let output = self.command(executable).args(arguments).output().unwrap();
            assert_code(&output, arguments, code);
            output
        }
        fn git(&self, arguments: &[&str]) -> Output {
            self.checked("git", arguments, 0)
        }
        fn cli(&self, arguments: &[&str], code: i32) -> Output {
            self.checked(env!("CARGO_BIN_EXE_overgit"), arguments, code)
        }
        fn cli_at(&self, directory: &Path, arguments: &[&str], code: i32) -> Output {
            let output = self
                .command(env!("CARGO_BIN_EXE_overgit"))
                .current_dir(directory)
                .args(arguments)
                .output()
                .unwrap();
            assert_code(&output, arguments, code);
            output
        }
        fn injected(&self, arguments: &[&str], mode: &str) -> Output {
            let binaries = self.temporary.path().join("bin");
            fs::create_dir(&binaries).unwrap();
            let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|path| path.join("git"))
                .find(|path| path.is_file())
                .unwrap();
            let wrapper = binaries.join("git");
            fs::write(&wrapper, b"#!/bin/sh\ncase \"$*\" in\n  *\"update-index --add --cacheinfo\"*)\n    if [ ! -e \"$AUDIT_ONCE\" ]; then\n      touch \"$AUDIT_ONCE\"\n      \"$AUDIT_REAL_GIT\" \"$@\" || exit $?\n      if [ \"$AUDIT_MODE\" = kill ]; then kill -KILL \"$PPID\"; exit 0; fi\n      : > \"$AUDIT_ROOT/.overgit/.git/index.lock\"\n      exit 1\n    fi\n    ;;\nesac\nexec \"$AUDIT_REAL_GIT\" \"$@\"\n").unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::join_paths(
                std::iter::once(binaries)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            self.command(env!("CARGO_BIN_EXE_overgit"))
                .args(arguments)
                .env("PATH", path)
                .env("AUDIT_REAL_GIT", real_git)
                .env("AUDIT_ONCE", self.temporary.path().join("injected"))
                .env("AUDIT_ROOT", &self.root)
                .env("AUDIT_MODE", mode)
                .output()
                .unwrap()
        }
    }
    fn assert_code(output: &Output, arguments: &[&str], code: i32) {
        assert_eq!(
            output.status.code(),
            Some(code),
            "{arguments:?}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn replacing_manifest_rescues_staged_only_fork_and_whiteout_after_gc() {
        let sandbox = Sandbox::new();
        let path = sandbox.root.join(".overgit/manifest.json");
        let original = fs::read(&path).unwrap();
        let staged = br#"{"version":2,"entries":{"lost-whiteout":{"kind":"delete","base":{"mode":"100644","data":"dW5pcXVlIGZvcmsgYnl0ZXM="}}}}"#;
        fs::write(&path, staged).unwrap();
        sandbox.cli(&["git", "add", ".overgit/manifest.json"], 0);
        let oid = String::from_utf8(
            sandbox
                .cli(&["git", "rev-parse", ":.overgit/manifest.json"], 0)
                .stdout,
        )
        .unwrap();
        fs::write(&path, original).unwrap();
        fs::write(sandbox.root.join("new"), b"new\n").unwrap();
        sandbox.cli(&["add", "new"], 0);
        assert!(!sandbox.root.join(".overgit/local/mutation.json").exists());
        sandbox.cli(&["git", "gc", "--prune=now"], 0);
        sandbox.cli(&["git", "cat-file", "-e", oid.trim()], 1);
        let rescued = fs::read_dir(sandbox.root.join(".overgit/local/backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|directory| {
                fs::read(directory.join("path")).unwrap() == b".overgit/manifest.json"
            })
            .expect("staged manifest must remain recoverable after garbage collection");
        assert_eq!(fs::read(rescued.join("content")).unwrap(), staged);
        let snapshot: serde_json::Value =
            serde_json::from_slice(&fs::read(rescued.join("snapshot.json")).unwrap()).unwrap();
        assert_eq!(snapshot["mode"], "100644");
    }

    #[test]
    fn recovery_rescues_staged_only_content_added_after_an_interrupted_mutation() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("private"), b"original unstaged bytes\n").unwrap();
        let failure = sandbox.injected(&["add", "private"], "fail");
        assert_eq!(failure.status.code(), Some(1));
        assert!(sandbox.root.join(".overgit/local/mutation.json").is_file());
        // Clear the interrupted Git process's lock, then stage a new version that
        // exists only in the index, without creating its worktree file.
        fs::remove_file(sandbox.root.join(".overgit/.git/index.lock")).unwrap();
        let staged = b"external staged-only after failure\n";
        let mut child = sandbox
            .command("git")
            .args(["--git-dir=.overgit/.git", "hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(staged).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oid = String::from_utf8(output.stdout).unwrap();
        sandbox.git(&[
            "--git-dir=.overgit/.git",
            "update-index",
            "--add",
            "--cacheinfo",
            "100644",
            oid.trim(),
            "later-staged",
        ]);
        assert!(!sandbox.root.join("later-staged").exists());
        sandbox.cli(&["doctor", "--fix"], 0);
        sandbox.git(&["--git-dir=.overgit/.git", "gc", "--prune=now"]);
        let mut rescued = false;
        for entry in fs::read_dir(sandbox.root.join(".overgit/local/backups")).unwrap() {
            let directory = entry.unwrap().path();
            if fs::read(directory.join("path")).unwrap() == b"later-staged" {
                assert_eq!(fs::read(directory.join("content")).unwrap(), staged);
                rescued = true;
            }
        }
        assert!(
            rescued,
            "rollback must preserve staged bytes, not only raw index references"
        );
        assert_eq!(
            fs::read(sandbox.root.join("private")).unwrap(),
            b"original unstaged bytes\n"
        );
        assert!(!sandbox.root.join(".overgit/local/mutation.json").exists());
    }

    #[test]
    fn killed_mid_add_can_recover_without_losing_original_bytes() {
        let sandbox = Sandbox::new();
        let before_index = fs::read(sandbox.root.join(".overgit/.git/index")).unwrap();
        let before_manifest = fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap();
        fs::write(
            sandbox.root.join("private"),
            b"original bytes before SIGKILL\n",
        )
        .unwrap();
        let killed = sandbox.injected(&["add", "private"], "kill");
        assert_eq!(killed.status.signal(), Some(9));
        assert!(sandbox.root.join(".overgit/local/mutation.json").is_file());
        sandbox.cli(&["doctor", "--fix"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("private")).unwrap(),
            b"original bytes before SIGKILL\n"
        );
        assert_eq!(
            fs::read(sandbox.root.join(".overgit/.git/index")).unwrap(),
            before_index
        );
        assert_eq!(
            fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap(),
            before_manifest
        );
        assert!(sandbox.cli(&["list", "--porcelain"], 0).stdout.is_empty());
        assert!(!sandbox.root.join(".overgit/local/mutation.json").exists());
    }

    #[test]
    fn recovering_one_worktree_preserves_sibling_exclusion_despite_invalid_manifest() {
        let sandbox = Sandbox::new();
        let sibling = sandbox.temporary.path().join("sibling");
        sandbox.cli(
            &[
                "worktree",
                "add",
                sibling.to_str().unwrap(),
                "-b",
                "sibling",
            ],
            0,
        );
        fs::write(sandbox.root.join(".overgit/manifest.json"), b"malformed\n").unwrap();
        sandbox.git(&[
            "--git-dir=.overgit/.git",
            "--work-tree=.",
            "add",
            "-f",
            ".overgit/manifest.json",
        ]);
        sandbox.git(&[
            "--git-dir=.overgit/.git",
            "commit",
            "-m",
            "damaged manifest",
        ]);
        let failed = sandbox.injected(&["doctor", "--fix", "--rebuild"], "fail");
        assert_eq!(failed.status.code(), Some(1));
        assert!(sandbox.root.join(".overgit/local/mutation.json").is_file());
        fs::remove_file(sandbox.root.join(".overgit/.git/index.lock")).unwrap();
        fs::write(sibling.join("sibling-private"), b"private sibling bytes\n").unwrap();
        sandbox.cli_at(&sibling, &["add", "sibling-private"], 0);
        sandbox.cli(&["doctor", "--fix"], 4);
        let staged = sandbox
            .command("git")
            .current_dir(&sibling)
            .args(["add", "--all"])
            .output()
            .unwrap();
        assert!(
            staged.status.success(),
            "{}",
            String::from_utf8_lossy(&staged.stderr)
        );
        let diff = sandbox
            .command("git")
            .current_dir(&sibling)
            .args(["diff", "--cached", "--name-only", "-z"])
            .output()
            .unwrap();
        assert!(diff.status.success());
        assert!(
            diff.stdout.is_empty(),
            "sibling private bytes leaked after unrelated recovery"
        );
        assert_eq!(
            fs::read(sibling.join("sibling-private")).unwrap(),
            b"private sibling bytes\n"
        );
        assert!(!sandbox.root.join(".overgit/local/mutation.json").exists());
    }

    #[test]
    fn failed_attach_with_a_locked_base_index_never_exposes_overlay_bytes() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("base"), b"private overlay\n").unwrap();
        sandbox.cli(&["add", "base"], 0);
        sandbox.cli(&["commit", "-m", "private overlay"], 0);
        sandbox.cli(&["detach"], 0);
        fs::write(
            sandbox.root.join(".git/index.lock"),
            b"held by another Git process\n",
        )
        .unwrap();
        sandbox.cli(&["attach"], 1);
        assert_eq!(fs::read(sandbox.root.join("base")).unwrap(), b"base\n");
        assert!(sandbox.root.join(".overgit/local/detached.json").exists());
        fs::remove_file(sandbox.root.join(".git/index.lock")).unwrap();
        sandbox.git(&["add", "--all"]);
        assert!(
            sandbox
                .git(&["diff", "--cached", "--name-only"])
                .stdout
                .is_empty()
        );
        sandbox.cli(&["attach"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("base")).unwrap(),
            b"private overlay\n"
        );
    }
}
