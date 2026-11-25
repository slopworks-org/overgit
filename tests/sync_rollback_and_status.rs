#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::{
        fs,
        path::PathBuf,
        process::{Command, Output},
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
            sandbox.write("f", b"base\n");
            sandbox.git(&["add", "f"]);
            sandbox.git(&["commit", "-m", "base"]);
            sandbox.cli(&["init"], 0);
            sandbox
        }

        fn command(&self, program: &str) -> Command {
            let mut command = Command::new(program);
            command
                .current_dir(&self.root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", self.temporary.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "Regression Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Regression Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
            command
        }

        fn git(&self, args: &[&str]) -> Vec<u8> {
            let output = self.command("git").args(args).output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn cli(&self, args: &[&str], expected: i32) -> Output {
            let output = self
                .command(env!("CARGO_BIN_EXE_overgit"))
                .args(args)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected),
                "overgit {args:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        fn write(&self, path: &str, data: &[u8]) {
            let destination = self.root.join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, data).unwrap();
        }
    }

    #[test]
    fn sync_failure_during_apply_rolls_back_automatically() {
        let sandbox = Sandbox::new();
        sandbox.write("f", b"a\nb\nc\n");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "three lines"]);
        sandbox.write("f", b"ours\nb\nc\n");
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["commit", "-m", "own f"], 0);
        sandbox.cli(&["detach"], 0);
        sandbox.write("f", b"a\nb\ntheirs\n");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "upstream"]);
        sandbox.cli(&["attach"], 0);

        // Clear skip-worktree so sync must update the base index, then lock .git/index.lock.
        sandbox.git(&["update-index", "--no-skip-worktree", "f"]);
        let lock = sandbox.root.join(".git/index.lock");
        fs::write(&lock, b"stale lock\n").unwrap();

        let failed = sandbox.cli(&["sync"], 1);
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(
            stderr.contains("rollback") || stderr.contains("rolled back"),
            "expected rollback diagnostic, got: {stderr}"
        );
        fs::remove_file(&lock).unwrap();
        if sandbox.root.join(".overgit/local/sync.json").exists() {
            sandbox.cli(&["sync", "--abort"], 0);
        }
        sandbox.cli(&["sync"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("f")).unwrap(),
            b"ours\nb\ntheirs\n"
        );
    }

    #[test]
    fn sync_rolls_back_cleanly_when_manifest_save_or_worktree_fails() {
        let sandbox = Sandbox::new();
        sandbox.write("f", b"a\nb\nc\n");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "three lines"]);
        sandbox.write("f", b"ours\nb\nc\n");
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["commit", "-m", "own f"], 0);
        sandbox.cli(&["detach"], 0);
        sandbox.write("f", b"a\nb\ntheirs\n");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "upstream"]);
        sandbox.cli(&["attach"], 0);

        // Lock the overlay index right before sync fails in stage(), while base index is unlocked.
        let lock = sandbox.root.join(".overgit/.git/index.lock");
        fs::write(&lock, b"stale overlay lock\n").unwrap();
        let failed = sandbox.cli(&["sync"], 1);
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(
            stderr.contains("sync --abort"),
            "incomplete rollback should direct user to sync --abort, got: {stderr}"
        );
        fs::remove_file(&lock).unwrap();
        sandbox.cli(&["sync", "--abort"], 0);
        assert!(!sandbox.root.join(".overgit/local/sync.json").exists());
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"ours\nb\nc\n");
    }

    #[test]
    fn interrupted_sync_reports_abort_guidance_in_commands_and_status() {
        let sandbox = Sandbox::new();
        sandbox.write("f", b"ours\n");
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["detach"], 0);
        sandbox.write("f", b"theirs\n");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "upstream"]);
        sandbox.cli(&["attach"], 0);
        sandbox.cli(&["sync"], 3);

        let sync_path = sandbox.root.join(".overgit/local/sync.json");
        let mut state: Value = serde_json::from_slice(&fs::read(&sync_path).unwrap()).unwrap();
        state["completed"] = Value::Bool(false);
        state["pending"] = serde_json::json!({});
        fs::write(&sync_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

        let retry = sandbox.cli(&["sync"], 1);
        let retry_err = String::from_utf8_lossy(&retry.stderr);
        assert!(
            retry_err.contains("sync was interrupted")
                && retry_err.contains("overgit sync --abort"),
            "unexpected sync error: {retry_err}"
        );
        assert!(!retry_err.contains("resolve and continue"));

        let status = sandbox.cli(&["status"], 0);
        let status_out = String::from_utf8_lossy(&status.stdout);
        assert!(
            status_out.contains("sync was interrupted")
                && status_out.contains("overgit sync --abort"),
            "unexpected status output:\n{status_out}"
        );
        assert!(
            !status_out.contains("info sync-in-progress"),
            "interrupted sync should not emit contradictory sync-in-progress info:\n{status_out}"
        );

        sandbox.cli(&["sync", "--abort"], 0);
        assert!(!sync_path.exists());
    }

    #[test]
    fn status_summarizes_pull_blocked_without_duplicate_warnings() {
        let sandbox = Sandbox::new();
        let remote = sandbox.temporary.path().join("remote.git");
        let init_remote = Command::new("git")
            .args([
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(init_remote.status.success());

        let names: Vec<String> = (0..15).map(|i| format!("doc_{i:02}.md")).collect();
        for name in &names {
            sandbox.write(name, b"v1\n");
        }
        sandbox.git(&["add", "--all"]);
        sandbox.git(&["commit", "-m", "add docs"]);
        sandbox.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        sandbox.git(&["push", "-u", "origin", "main"]);

        let mut rm_args = vec!["rm"];
        rm_args.extend(names.iter().map(String::as_str));
        sandbox.cli(&rm_args, 0);
        sandbox.cli(&["commit", "-m", "hide docs"], 0);

        let writer = sandbox.temporary.path().join("writer");
        let clone = sandbox
            .command("git")
            .args(["clone", remote.to_str().unwrap(), writer.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(clone.status.success());
        for name in &names {
            fs::write(writer.join(name), b"v2\n").unwrap();
        }
        let commit = sandbox
            .command("git")
            .current_dir(&writer)
            .args(["commit", "-am", "update docs"])
            .output()
            .unwrap();
        assert!(commit.status.success());
        let push = sandbox
            .command("git")
            .current_dir(&writer)
            .args(["push"])
            .output()
            .unwrap();
        assert!(push.status.success());

        sandbox.git(&["fetch", "origin"]);

        let status = sandbox.cli(&["status"], 0);
        let text = String::from_utf8_lossy(&status.stdout);
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("base upstream touches owned path "))
                .count(),
            10,
            "expected at most 10 pull_blocked lines in:\n{text}"
        );
        assert!(
            text.contains("... and 5 more owned paths touched by base upstream"),
            "expected summary line in:\n{text}"
        );
        assert!(
            !text.contains("warning base-pull-owned"),
            "duplicate base-pull-owned warnings should be omitted:\n{text}"
        );

        let porcelain: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert_eq!(porcelain["pull_blocked"].as_array().unwrap().len(), 15);
        assert_eq!(
            porcelain["problems"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|problem| problem["code"] == "base-pull-owned")
                .count(),
            15
        );
    }
}
