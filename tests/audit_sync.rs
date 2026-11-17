#[cfg(test)]
mod tests {
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
            sandbox.write("f", b"fork\n");
            sandbox.git(&["add", "f"]);
            sandbox.git(&["commit", "-m", "base"]);
            sandbox.cli(&["init"], 0);
            sandbox
        }
        fn command(&self, program: &str, args: &[&str]) -> Output {
            Command::new(program)
                .current_dir(&self.root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", self.temporary.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "Audit Sync")
                .env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
                .env("GIT_COMMITTER_NAME", "Audit Sync")
                .env("GIT_COMMITTER_EMAIL", "audit@example.invalid")
                .args(args)
                .output()
                .unwrap()
        }
        fn git(&self, args: &[&str]) -> Vec<u8> {
            let output = self.command("git", args);
            assert!(
                output.status.success(),
                "Git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }
        fn cli(&self, args: &[&str], expected: i32) -> Output {
            let output = self.command(env!("CARGO_BIN_EXE_overgit"), args);
            assert_eq!(
                output.status.code(),
                Some(expected),
                "overgit {args:?}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }
        fn write(&self, path: &str, data: &[u8]) {
            fs::write(self.root.join(path), data).unwrap();
        }
        fn conflict(&self) {
            self.write("f", b"private overlay\n");
            self.cli(&["add", "f"], 0);
            self.cli(&["detach"], 0);
            self.write("f", b"upstream\n");
            self.git(&["add", "f"]);
            self.git(&["commit", "-m", "upstream"]);
            self.cli(&["attach"], 0);
        }
        fn collision(&self) {
            self.write("x", b"saved private overlay\n");
            self.cli(&["add", "x"], 0);
            self.write("x", b"upstream addition\n");
            self.git(&["add", "--force", "x"]);
            self.git(&["commit", "-m", "upstream adds x"]);
        }
        fn backups(&self) -> Vec<Vec<u8>> {
            fs::read_dir(self.root.join(".overgit/local/backups"))
                .unwrap()
                .map(|entry| fs::read(entry.unwrap().path().join("content")).unwrap())
                .collect()
        }
        fn state(&self) -> Vec<Vec<u8>> {
            [
                ".overgit/local/sync.json",
                ".overgit/.git/index",
                ".overgit/manifest.json",
                ".git/info/exclude",
                "f",
            ]
            .iter()
            .map(|path| fs::read(self.root.join(path)).unwrap())
            .collect()
        }
    }

    #[test]
    fn abort_rescues_unstaged_manifest_fork_and_whiteout_bytes() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        let path = sandbox.root.join(".overgit/manifest.json");
        let original = fs::read(&path).unwrap();
        sandbox.cli(&["sync"], 3);
        let mut edited: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edited["entries"]["unstaged-whiteout"] = serde_json::json!({
            "kind": "delete",
            "base": {"mode": "100644", "data": "dW5pcXVlIHVuc3RhZ2VkIGZvcms="}
        });
        let edited = serde_json::to_vec(&edited).unwrap();
        fs::write(&path, &edited).unwrap();
        sandbox.cli(&["sync", "--abort"], 0);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!sandbox.root.join(".overgit/local/sync.json").exists());
        sandbox.cli(&["git", "gc", "--prune=now"], 0);
        assert!(sandbox.backups().contains(&edited));
        let rescued = fs::read_dir(sandbox.root.join(".overgit/local/backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|directory| fs::read(directory.join("content")).unwrap() == edited)
            .unwrap();
        assert_eq!(
            fs::read(rescued.join("path")).unwrap(),
            b".overgit/manifest.json"
        );
    }

    #[test]
    fn adopt_uses_saved_overlay_after_base_checkout_overwrites_an_addition() {
        let sandbox = Sandbox::new();
        sandbox.collision();
        sandbox.cli(&["sync"], 3);
        sandbox.cli(&["resolve", "--adopt", "x"], 3);
        let merged = fs::read_to_string(sandbox.root.join("x")).unwrap();
        assert!(merged.contains("saved private overlay"));
        assert!(merged.contains("upstream addition"));
        assert!(merged.contains("<<<<<<<"));
    }

    #[test]
    fn collision_drop_rescues_indexed_overlay_when_worktree_contains_base() {
        let sandbox = Sandbox::new();
        sandbox.collision();
        sandbox.cli(&["sync"], 3);
        sandbox.cli(&["resolve", "--drop", "x"], 0);
        assert!(
            sandbox
                .backups()
                .contains(&b"saved private overlay\n".to_vec())
        );
        assert_eq!(
            fs::read(sandbox.root.join("x")).unwrap(),
            b"upstream addition\n"
        );
    }

    #[test]
    fn collision_drop_preserves_indexed_and_unstaged_overlay_versions() {
        let sandbox = Sandbox::new();
        sandbox.collision();
        sandbox.write("x", b"unstaged private edit\n");
        sandbox.cli(&["sync"], 3);
        sandbox.cli(&["resolve", "--drop", "x"], 0);
        let backups = sandbox.backups();
        assert!(backups.contains(&b"saved private overlay\n".to_vec()));
        assert!(backups.contains(&b"unstaged private edit\n".to_vec()));
    }

    #[test]
    fn abort_restores_original_skip_and_assume_flags_without_normalizing() {
        for (skip, assume) in [(false, false), (false, true), (true, false), (true, true)] {
            let sandbox = Sandbox::new();
            sandbox.conflict();
            sandbox.git(&[
                "update-index",
                if skip {
                    "--skip-worktree"
                } else {
                    "--no-skip-worktree"
                },
                "--",
                "f",
            ]);
            sandbox.git(&[
                "update-index",
                if assume {
                    "--assume-unchanged"
                } else {
                    "--no-assume-unchanged"
                },
                "--",
                "f",
            ]);
            let before = sandbox.git(&["ls-files", "-v", "-z", "--", "f"]);
            sandbox.cli(&["sync"], 3);
            sandbox.cli(&["sync", "--abort"], 0);
            assert_eq!(sandbox.git(&["ls-files", "-v", "-z", "--", "f"]), before);
        }
    }

    #[test]
    fn pending_sync_dry_run_reports_attention_without_mutating_any_snapshot() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.cli(&["sync"], 3);
        let before = sandbox.state();
        let flags = sandbox.git(&["ls-files", "-v", "-z"]);
        sandbox.cli(&["sync", "--dry-run"], 4);
        assert_eq!(sandbox.state(), before);
        assert_eq!(sandbox.git(&["ls-files", "-v", "-z"]), flags);
    }

    #[test]
    fn missing_sync_input_is_attention_in_dry_run_and_real_sync() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        fs::remove_file(sandbox.root.join("f")).unwrap();
        sandbox.cli(&["sync", "--dry-run"], 4);
        let output = sandbox.cli(&["sync"], 4);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("already up to date"));
        assert!(!sandbox.root.join(".overgit/local/sync.json").exists());
        assert!(!sandbox.root.join("f").exists());
    }
    #[test]
    fn abort_keeps_a_split_index_usable_after_its_shared_file_disappears() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.cli(&["git", "update-index", "--split-index"], 0);
        let raw = fs::read(sandbox.root.join(".overgit/.git/index")).unwrap();
        let shared = sandbox
            .cli(&["git", "rev-parse", "--shared-index-path"], 0)
            .stdout;
        let shared = PathBuf::from(String::from_utf8(shared).unwrap().trim());
        let shared = if shared.is_absolute() {
            shared
        } else {
            sandbox.root.join(shared)
        };
        assert!(shared.exists());
        sandbox.cli(&["sync"], 3);
        fs::remove_file(shared).unwrap();
        sandbox.cli(&["sync", "--abort"], 0);
        assert_eq!(
            fs::read(sandbox.root.join(".overgit/.git/index")).unwrap(),
            raw
        );
        sandbox.cli(&["git", "ls-files", "--stage"], 0);
        assert_eq!(
            sandbox.cli(&["git", "show", ":f"], 0).stdout,
            b"private overlay\n"
        );
    }
    #[test]
    fn pending_binary_snapshots_are_inspectable_without_changing_the_sync() {
        use base64::Engine as _;
        let sandbox = Sandbox::new();
        sandbox.write("f", b"fork\0binary");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "binary base"]);
        sandbox.write("f", b"private\0binary");
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["detach"], 0);
        sandbox.write("f", b"upstream\0binary");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "binary upstream"]);
        sandbox.cli(&["attach"], 0);
        sandbox.cli(&["sync"], 3);
        let before = sandbox.state();
        let report: serde_json::Value =
            serde_json::from_slice(&sandbox.cli(&["sync", "--inspect", "f"], 0).stdout).unwrap();
        assert_eq!(report["situation"], "binary");
        assert_eq!(
            report["ours"]["data"],
            base64::engine::general_purpose::STANDARD.encode(b"private\0binary")
        );
        assert_eq!(
            report["upstream"]["data"],
            base64::engine::general_purpose::STANDARD.encode(b"upstream\0binary")
        );
        assert_eq!(sandbox.state(), before);
    }

    #[test]
    fn abort_does_not_rollback_unrelated_new_base_staging() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.git(&["update-index", "--no-skip-worktree", "--", "f"]);
        sandbox.cli(&["sync"], 3);
        sandbox.write("unrelated", b"new staged work\n");
        sandbox.git(&["add", "unrelated"]);
        let staged = sandbox.git(&["diff", "--cached", "--binary"]);
        sandbox.cli(&["sync", "--abort"], 0);
        assert_eq!(sandbox.git(&["diff", "--cached", "--binary"]), staged);
        assert_eq!(sandbox.git(&["ls-files", "-v", "-z", "--", "f"]), b"H f\0");
    }
    #[test]
    fn sync_uses_saved_override_when_worktree_was_replaced_by_base_checkout() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.git(&["update-index", "--no-skip-worktree", "--", "f"]);
        sandbox.git(&["checkout", "--", "f"]);
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"upstream\n");
        sandbox.cli(&["sync"], 3);
        let markers = fs::read_to_string(sandbox.root.join("f")).unwrap();
        assert!(markers.contains("private overlay"));
        assert!(markers.contains("upstream"));
        assert!(markers.contains("<<<<<<<"));
    }
    #[test]
    fn abort_recreates_original_staged_blob_pruned_after_a_clean_sibling_merge() {
        let sandbox = Sandbox::new();
        sandbox.write("clean", b"a\nb\nc\nd\n");
        sandbox.git(&["add", "clean"]);
        sandbox.git(&["commit", "-m", "clean base"]);
        sandbox.write("f", b"private conflict\n");
        sandbox.write("clean", b"ours\nb\nc\nd\n");
        sandbox.cli(&["add", "f", "clean"], 0);
        let original = sandbox.cli(&["git", "rev-parse", ":clean"], 0).stdout;
        let original = String::from_utf8(original).unwrap();
        sandbox.cli(&["detach"], 0);
        sandbox.write("f", b"upstream conflict\n");
        sandbox.write("clean", b"a\nb\nc\ntheirs\n");
        sandbox.git(&["add", "f", "clean"]);
        sandbox.git(&["commit", "-m", "base moves"]);
        sandbox.cli(&["attach"], 0);
        sandbox.cli(&["sync"], 3);
        sandbox.cli(&["git", "gc", "--prune=now"], 0);
        let absent = sandbox.command(
            env!("CARGO_BIN_EXE_overgit"),
            &["git", "cat-file", "-e", original.trim()],
        );
        assert!(
            !absent.status.success(),
            "fixture must actually prune the old uncommitted blob"
        );
        sandbox.cli(&["sync", "--abort"], 0);
        assert_eq!(
            sandbox.cli(&["git", "show", ":clean"], 0).stdout,
            b"ours\nb\nc\nd\n"
        );
    }

    #[test]
    fn abort_rescues_new_and_replaced_staged_only_versions_from_external_git() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.cli(&["sync"], 3);
        sandbox.write("f", b"new staged resolution\n");
        sandbox.write("fresh", b"new staged-only path\n");
        sandbox.cli(&["git", "add", "--force", "f", "fresh"], 0);
        fs::remove_file(sandbox.root.join("f")).unwrap();
        fs::remove_file(sandbox.root.join("fresh")).unwrap();
        sandbox.cli(&["sync", "--abort"], 0);
        let backups = sandbox.backups();
        assert!(backups.contains(&b"new staged resolution\n".to_vec()));
        assert!(backups.contains(&b"new staged-only path\n".to_vec()));
        assert_eq!(
            sandbox.cli(&["git", "show", ":f"], 0).stdout,
            b"private overlay\n"
        );
    }

    #[test]
    fn abort_recreates_pruned_index_payloads_outside_the_selected_sync_paths() {
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.write("outside", b"outside selected sync\n");
        sandbox.cli(&["add", "outside"], 0);
        let original =
            String::from_utf8(sandbox.cli(&["git", "rev-parse", ":outside"], 0).stdout).unwrap();
        sandbox.cli(&["sync", "--only", "f"], 3);
        sandbox.cli(
            &["git", "update-index", "--force-remove", "--", "outside"],
            0,
        );
        sandbox.cli(&["git", "gc", "--prune=now"], 0);
        assert!(
            !sandbox
                .command(
                    env!("CARGO_BIN_EXE_overgit"),
                    &["git", "cat-file", "-e", original.trim()]
                )
                .status
                .success()
        );
        sandbox.cli(&["sync", "--abort"], 0);
        assert_eq!(
            sandbox.cli(&["git", "show", ":outside"], 0).stdout,
            b"outside selected sync\n"
        );
    }
    #[test]
    fn abort_refuses_an_external_unmerged_overlay_index_before_changing_work() {
        use std::io::Write as _;
        use std::process::Stdio;
        let sandbox = Sandbox::new();
        sandbox.conflict();
        sandbox.cli(&["sync"], 3);
        let oid = String::from_utf8(sandbox.cli(&["git", "rev-parse", ":f"], 0).stdout).unwrap();
        let oid = oid.trim();
        let stages = format!(
            "0 {}\tf\n100644 {oid} 1\tf\n100644 {oid} 2\tf\n100644 {oid} 3\tf\n",
            "0".repeat(oid.len())
        );
        let mut child = Command::new("git")
            .current_dir(&sandbox.root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", sandbox.temporary.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "--git-dir=.overgit/.git",
                "--work-tree=.",
                "update-index",
                "--index-info",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stages.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let before = sandbox.state();
        sandbox.cli(&["sync", "--abort"], 1);
        assert_eq!(sandbox.state(), before);
        assert!(!sandbox.root.join(".overgit/.git/index.lock").exists());
    }
}
