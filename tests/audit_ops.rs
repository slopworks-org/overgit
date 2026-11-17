#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        process::{Command, Output},
    };
    use tempfile::TempDir;
    struct Sandbox {
        temp: TempDir,
        root: PathBuf,
    }
    impl Sandbox {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("base");
            fs::create_dir(&root).unwrap();
            let sandbox = Self { temp, root };
            sandbox.git(&["init", "--initial-branch=main"]);
            fs::write(sandbox.root.join("f"), b"base\n").unwrap();
            sandbox.git(&["add", "--all"]);
            sandbox.git(&["commit", "-m", "base"]);
            sandbox
        }
        fn command(&self, executable: &str, args: &[&str]) -> Output {
            Command::new(executable)
                .current_dir(&self.root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", self.temp.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "Audit Test")
                .env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
                .env("GIT_COMMITTER_NAME", "Audit Test")
                .env("GIT_COMMITTER_EMAIL", "audit@example.invalid")
                .env("GIT_EDITOR", "true")
                .args(args)
                .output()
                .unwrap()
        }
        fn git(&self, args: &[&str]) -> Output {
            let out = self.command("git", args);
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }
        fn cli(&self, args: &[&str], code: i32) -> Output {
            let out = self.command(env!("CARGO_BIN_EXE_overgit"), args);
            assert_eq!(
                out.status.code(),
                Some(code),
                "overgit {args:?}: {} / {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }
        fn init(&self) {
            self.cli(&["init"], 0);
        }
        fn saved(&self) -> Vec<u8> {
            self.cli(&["git", "show", ":f"], 0).stdout
        }
        fn backup_contains(&self, expected: &[u8]) -> bool {
            fn visit(path: &Path, expected: &[u8]) -> bool {
                fs::read_dir(path).is_ok_and(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        let path = entry.path();
                        if path.is_dir() {
                            visit(&path, expected)
                        } else {
                            fs::read(&path).is_ok_and(|bytes| bytes == expected)
                        }
                    })
                })
            }
            visit(&self.root.join(".overgit/local/backups"), expected)
        }
    }
    #[test]
    fn detach_keeps_staged_overlay_when_base_checkout_overwrote_worktree() {
        let sandbox = Sandbox::new();
        sandbox.init();
        fs::write(sandbox.root.join("f"), b"staged private\n").unwrap();
        sandbox.cli(&["add", "f"], 0);
        sandbox.git(&["update-index", "--no-skip-worktree", "--", "f"]);
        sandbox.git(&["checkout", "--", "f"]);
        sandbox.cli(&["detach"], 0);
        assert_eq!(sandbox.saved(), b"staged private\n");
        sandbox.cli(&["attach"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("f")).unwrap(),
            b"staged private\n"
        );
    }
    #[test]
    fn missing_staged_only_content_is_rescued_for_rm_and_restore() {
        for action in ["rm", "restore"] {
            let sandbox = Sandbox::new();
            sandbox.init();
            fs::write(sandbox.root.join("secret"), b"only indexed copy\n").unwrap();
            sandbox.cli(&["add", "secret"], 0);
            fs::remove_file(sandbox.root.join("secret")).unwrap();
            sandbox.cli(&[action, "secret"], 0);
            assert!(
                sandbox.backup_contains(b"only indexed copy\n"),
                "missing backup for {action}"
            );
        }
    }
    #[test]
    fn ownership_preserves_non_utf8_exclude_and_never_leaks_private_file() {
        let sandbox = Sandbox::new();
        sandbox.init();
        let path = sandbox.root.join(".git/info/exclude");
        let mut original = fs::read(&path).unwrap();
        original.extend_from_slice(b"# local \xff\n");
        fs::write(&path, &original).unwrap();
        fs::write(sandbox.root.join("private"), b"secret\n").unwrap();
        sandbox.cli(&["add", "private"], 0);
        assert!(
            fs::read(path)
                .unwrap()
                .windows(10)
                .any(|bytes| bytes == b"# local \xff\n")
        );
        sandbox.git(&["add", "--all"]);
        assert!(
            sandbox
                .git(&["diff", "--cached", "--name-only"])
                .stdout
                .is_empty()
        );
    }
    #[test]
    fn doctor_retains_content_edits_with_executable_mode_changes() {
        let sandbox = Sandbox::new();
        sandbox.init();
        fs::write(sandbox.root.join("f"), b"saved\n").unwrap();
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["commit", "-m", "saved"], 0);
        fs::write(sandbox.root.join("f"), b"edited\n").unwrap();
        fs::set_permissions(sandbox.root.join("f"), fs::Permissions::from_mode(0o755)).unwrap();
        let output = sandbox.command(env!("CARGO_BIN_EXE_overgit"), &["doctor", "--fix"]);
        assert!(matches!(output.status.code(), Some(0 | 4)));
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"edited\n");
        assert_ne!(
            fs::metadata(sandbox.root.join("f"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    #[test]
    fn doctor_reports_absent_overlay_and_base_operation_readiness() {
        let sandbox = Sandbox::new();
        let report = sandbox.cli(&["doctor", "--porcelain"], 4);
        assert!(String::from_utf8_lossy(&report.stdout).contains("overlay"));
        sandbox.init();
        sandbox.git(&["config", "core.sparseCheckout", "true"]);
        let sparse = sandbox.cli(&["doctor", "--porcelain"], 4);
        assert!(String::from_utf8_lossy(&sparse.stdout).contains("sparse"));
        sandbox.git(&["config", "core.sparseCheckout", "false"]);
        fs::write(
            sandbox.root.join(".git/MERGE_HEAD"),
            sandbox.git(&["rev-parse", "HEAD"]).stdout,
        )
        .unwrap();
        let busy = sandbox.cli(&["doctor", "--porcelain"], 4);
        assert!(String::from_utf8_lossy(&busy.stdout).contains("operation"));
    }
    #[test]
    fn doctor_falls_back_from_corrupt_index_manifest_to_valid_head() {
        let sandbox = Sandbox::new();
        sandbox.init();
        fs::write(sandbox.root.join("f"), b"private\n").unwrap();
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["commit", "-m", "saved"], 0);
        fs::write(sandbox.root.join(".overgit/manifest.json"), b"broken\n").unwrap();
        sandbox.cli(&["git", "add", "--force", ".overgit/manifest.json"], 0);
        sandbox.cli(&["doctor", "--fix"], 0);
        assert!(
            String::from_utf8_lossy(&sandbox.cli(&["list", "--porcelain"], 0).stdout).contains('f')
        );
        assert!(sandbox.backup_contains(b"broken\n"));
    }
    #[test]
    fn lost_detach_marker_does_not_restage_base_over_saved_overlay() {
        let sandbox = Sandbox::new();
        sandbox.init();
        fs::write(sandbox.root.join("f"), b"uncommitted overlay\n").unwrap();
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["detach"], 0);
        fs::remove_file(sandbox.root.join(".overgit/local/detached.json")).unwrap();
        sandbox.cli(&["detach"], 0);
        sandbox.cli(&["attach"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("f")).unwrap(),
            b"uncommitted overlay\n"
        );
    }
    #[test]
    fn doctor_preserves_raw_overlay_exclude_comments_while_repairing_patterns() {
        let sandbox = Sandbox::new();
        sandbox.init();
        let file = sandbox.root.join(".overgit/.git/info/exclude");
        fs::write(&file, b"# raw \xff\n").unwrap();
        sandbox.cli(&["doctor", "--fix"], 0);
        assert!(fs::read(file).unwrap().starts_with(b"# raw \xff\n"));
    }
    #[test]
    fn portable_manifest_publication_contains_base_snapshot_as_documented() {
        use base64::Engine as _;
        let sandbox = Sandbox::new();
        sandbox.init();
        sandbox.cli(&["rm", "f"], 0);
        sandbox.cli(&["commit", "-m", "whiteout"], 0);
        let bytes = sandbox
            .cli(&["git", "show", "HEAD:.overgit/manifest.json"], 0)
            .stdout;
        let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let encoded = manifest["entries"]["f"]["base"]["data"].as_str().unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            b"base\n"
        );
    }
}
