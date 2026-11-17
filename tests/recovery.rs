#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::{PermissionsExt as _, symlink},
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
        time::{Duration, Instant},
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
            let result = Self { temp, root };
            result.git(&["init", "--initial-branch=main"]);
            fs::write(result.root.join("f"), b"base\n").unwrap();
            result.git(&["add", "f"]);
            result.git(&["commit", "-m", "base"]);
            result.cli(&["init"], 0);
            result
        }
        fn command(&self, program: &str) -> Command {
            let mut cmd = Command::new(program);
            cmd.current_dir(&self.root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", self.temp.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Recovery Test")
                .env("GIT_AUTHOR_EMAIL", "recovery@example.invalid")
                .env("GIT_COMMITTER_NAME", "Recovery Test")
                .env("GIT_COMMITTER_EMAIL", "recovery@example.invalid");
            cmd
        }
        fn git(&self, args: &[&str]) -> Output {
            let out = self.command("git").args(args).output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }
        fn cli(&self, args: &[&str], code: i32) -> Output {
            let out = self
                .command(env!("CARGO_BIN_EXE_overgit"))
                .args(args)
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(code),
                "{args:?}\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }
        fn backup(&self, name: &str, saved: &[u8], current: &[u8]) -> String {
            fs::write(self.root.join(name), saved).unwrap();
            self.cli(&["add", name], 0);
            fs::write(self.root.join(name), current).unwrap();
            self.cli(&["add", "--force", name], 0);
            self.cli(&["commit", "-m", "current"], 0);
            self.find(saved)
        }
        fn find(&self, bytes: &[u8]) -> String {
            fs::read_dir(self.root.join(".overgit/local/backups"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| fs::read(path.join("content")).ok().as_deref() == Some(bytes))
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        }
        fn state(&self) -> Vec<Vec<u8>> {
            [
                ".overgit/manifest.json",
                ".overgit/.git/index",
                ".git/index",
                ".git/info/exclude",
            ]
            .iter()
            .map(|name| fs::read(self.root.join(name)).unwrap())
            .collect()
        }
        fn entry(&self, id: &str) -> PathBuf {
            self.root.join(".overgit/local/backups").join(id)
        }
        fn count(&self) -> usize {
            fs::read_dir(self.root.join(".overgit/local/backups"))
                .unwrap()
                .count()
        }
    }
    fn files(directory: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_str().unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn snapshot_preview_dryrun_restore_and_noop_preserve_source_and_indexes() {
        let sandbox = Sandbox::new();
        let saved = b"saved\0binary\x1b[31m\n";
        let id = sandbox.backup("f", saved, b"current\n");
        let source = files(&sandbox.entry(&id));
        let state = sandbox.state();
        let count = sandbox.count();
        let listing: Value =
            serde_json::from_slice(&sandbox.cli(&["recovery", "list", "--porcelain"], 0).stdout)
                .unwrap();
        assert!(
            listing["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["id"] == id)
        );
        let preview = sandbox.cli(&["recovery", "inspect", &id, "--porcelain"], 0);
        assert!(!preview.stdout.contains(&27));
        assert!(preview.stdout.is_ascii());
        let report: Value = serde_json::from_slice(&preview.stdout).unwrap();
        assert_eq!(report["saved"]["bytes"], saved.len());
        assert_eq!(report["restore"]["would_rescue_current"], true);
        sandbox.cli(&["recovery", "restore", &id, "--dry-run"], 4);
        assert_eq!(sandbox.state(), state);
        assert_eq!(sandbox.count(), count);
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"current\n");
        assert!(!sandbox.root.join(".overgit/local/mutation.json").exists());
        sandbox.cli(&["recovery", "restore", &id], 0);
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), saved);
        assert_eq!(sandbox.state(), state);
        assert_eq!(files(&sandbox.entry(&id)), source);
        assert_eq!(
            fs::read(sandbox.entry(&sandbox.find(b"current\n")).join("content")).unwrap(),
            b"current\n"
        );
        let count = sandbox.count();
        sandbox.cli(&["recovery", "restore", &id], 0);
        sandbox.cli(&["recovery", "restore", &id, "--dry-run"], 0);
        assert_eq!(sandbox.count(), count);
    }

    #[test]
    fn metadata_requires_explicit_export_and_inspection_survives_pending_state() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("one"), b"one\n").unwrap();
        sandbox.cli(&["add", "one"], 0);
        let saved = fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap();
        fs::write(sandbox.root.join("two"), b"two\n").unwrap();
        sandbox.cli(&["add", "two"], 0);
        let id = sandbox.find(&saved);
        let state = sandbox.state();
        sandbox.cli(&["recovery", "restore", &id], 1);
        sandbox.cli(
            &["recovery", "restore", &id, "--to", ".overgit/manifest.json"],
            1,
        );
        sandbox.cli(&["recovery", "restore", &id, "--to", "recovered.json"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("recovered.json")).unwrap(),
            saved
        );
        assert_eq!(sandbox.state(), state);
        fs::write(
            sandbox.root.join(".overgit/local/mutation.json"),
            b"pending journal",
        )
        .unwrap();
        sandbox.cli(&["recovery", "list"], 0);
        sandbox.cli(&["recovery", "inspect", &id], 0);
        sandbox.cli(
            &[
                "recovery",
                "restore",
                &id,
                "--to",
                "other.json",
                "--dry-run",
            ],
            1,
        );
        fs::remove_file(sandbox.root.join(".overgit/.git/index")).unwrap();
        sandbox.cli(&["recovery", "list"], 0);
        sandbox.cli(&["recovery", "inspect", &id], 0);
    }

    #[test]
    fn directory_archive_leaf_and_symlink_restore_do_not_follow_source_links() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("owned"), b"saved owned\n").unwrap();
        sandbox.cli(&["add", "owned"], 0);
        sandbox.cli(&["commit", "-m", "owned"], 0);
        fs::remove_file(sandbox.root.join("owned")).unwrap();
        fs::create_dir(sandbox.root.join("owned")).unwrap();
        fs::write(sandbox.root.join("owned/tool"), b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(
            sandbox.root.join("owned/tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let outside = sandbox.temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside\n").unwrap();
        symlink(&outside, sandbox.root.join("owned/link")).unwrap();
        sandbox.cli(&["apply", "--force"], 0);
        let entry = fs::read_dir(sandbox.root.join(".overgit/local/backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.join("content").is_dir())
            .unwrap();
        let id = entry.file_name().unwrap().to_str().unwrap();
        sandbox.cli(&["recovery", "inspect", id], 0);
        sandbox.cli(&["recovery", "restore", id], 1);
        sandbox.cli(&["recovery", "inspect", id, "--path", "link/secret"], 1);
        let state = sandbox.state();
        sandbox.cli(
            &[
                "recovery",
                "restore",
                id,
                "--path",
                "tool",
                "--to",
                "restored-tool",
            ],
            0,
        );
        assert_eq!(
            fs::metadata(sandbox.root.join("restored-tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        sandbox.cli(
            &[
                "recovery",
                "restore",
                id,
                "--path",
                "link",
                "--to",
                "restored-link",
            ],
            0,
        );
        assert_eq!(
            fs::read_link(sandbox.root.join("restored-link")).unwrap(),
            outside
        );
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside\n");
        assert_eq!(sandbox.state(), state);
        assert_eq!(
            fs::read(entry.join("content/tool")).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
    }

    #[test]
    fn damaged_sources_and_unsafe_destinations_fail_without_writes() {
        let sandbox = Sandbox::new();
        let id = sandbox.backup("f", b"saved\n", b"current\n");
        let state = sandbox.state();
        let count = sandbox.count();
        let outside = sandbox.temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("f"), b"outside\n").unwrap();
        symlink(&outside, sandbox.root.join("escape")).unwrap();
        for args in [
            vec!["recovery", "restore", &id, "--to", "escape/f"],
            vec!["recovery", "restore", &id, "--to", "../outside/f"],
            vec!["recovery", "inspect", "../backups"],
            vec!["recovery", "restore", &id, "--to", ".git/config"],
        ] {
            sandbox.cli(&args, 1);
        }
        fs::write(sandbox.entry(&id).join("content"), b"damaged payload\n").unwrap();
        sandbox.cli(&["recovery", "restore", &id, "--dry-run"], 1);
        sandbox.cli(&["recovery", "restore", &id], 1);
        fs::remove_file(sandbox.entry(&id).join("content")).unwrap();
        symlink(outside.join("f"), sandbox.entry(&id).join("content")).unwrap();
        sandbox.cli(&["recovery", "inspect", &id], 1);
        assert_eq!(sandbox.state(), state);
        assert_eq!(sandbox.count(), count);
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"current\n");
        assert_eq!(fs::read(outside.join("f")).unwrap(), b"outside\n");
    }

    #[test]
    fn interrupted_restore_retains_source_and_recovers_pre_restore_work() {
        let sandbox = Sandbox::new();
        let id = sandbox.backup("f", b"saved small\n", b"committed current\n");
        let source = files(&sandbox.entry(&id));
        let state = sandbox.state();
        let current = vec![b'x'; 32 * 1024 * 1024];
        fs::write(sandbox.root.join("f"), &current).unwrap();
        let mut child = sandbox
            .command(env!("CARGO_BIN_EXE_overgit"))
            .args(["recovery", "restore", &id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let journal = sandbox.root.join(".overgit/local/mutation.json");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !journal.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "restore completed before interruption"
            );
            assert!(Instant::now() < deadline, "restore journal did not appear");
            std::thread::sleep(Duration::from_millis(1));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        assert!(journal.exists());
        assert_eq!(files(&sandbox.entry(&id)), source);
        sandbox.cli(&["doctor", "--fix"], 0);
        assert!(!journal.exists());
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), current);
        assert_eq!(sandbox.state(), state);
        assert_eq!(files(&sandbox.entry(&id)), source);
    }
    #[test]
    fn real_aborted_pull_restores_selected_draft_without_resuming_session() {
        let sandbox = Sandbox::new();
        sandbox.backup("f", b"old overlay\n", b"shared overlay\n");
        let other = sandbox.temp.path().join("other");
        sandbox.git(&[
            "clone",
            sandbox.root.to_str().unwrap(),
            other.to_str().unwrap(),
        ]);
        let other_cli = |args: &[&str]| {
            let output = sandbox
                .command(env!("CARGO_BIN_EXE_overgit"))
                .current_dir(&other)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        other_cli(&[
            "clone",
            sandbox.root.join(".overgit/.git").to_str().unwrap(),
        ]);
        fs::write(sandbox.root.join("f"), b"local divergence\n").unwrap();
        sandbox.cli(&["commit", "-m", "local"], 0);
        fs::write(other.join("f"), b"remote divergence\n").unwrap();
        other_cli(&["commit", "-m", "remote"]);
        sandbox.cli(
            &[
                "pull",
                "--no-rebase",
                other.join(".overgit/.git").to_str().unwrap(),
            ],
            3,
        );
        fs::write(
            sandbox.root.join(".overgit/local/pull/tree/draft"),
            b"isolated draft\n",
        )
        .unwrap();
        sandbox.cli(&["pull", "--abort"], 0);
        let entry = fs::read_dir(sandbox.root.join(".overgit/local/backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("pull-aborted-")
            })
            .unwrap();
        let id = entry.file_name().unwrap().to_str().unwrap();
        let before = sandbox.state();
        sandbox.cli(&["recovery", "inspect", id], 0);
        sandbox.cli(
            &["recovery", "restore", id, "--path", "draft", "--dry-run"],
            4,
        );
        assert!(!sandbox.root.join("draft").exists());
        sandbox.cli(&["recovery", "restore", id, "--path", "draft"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("draft")).unwrap(),
            b"isolated draft\n"
        );
        assert_eq!(
            fs::read(entry.join("tree/draft")).unwrap(),
            b"isolated draft\n"
        );
        assert_eq!(sandbox.state(), before);
        assert!(!sandbox.root.join(".overgit/local/pull").exists());
    }
    #[test]
    fn invalid_missing_parent_destinations_fail_before_journaling() {
        let sandbox = Sandbox::new();
        let id = sandbox.backup("f", b"saved\n", b"current\n");
        let before = sandbox.state();
        let source = files(&sandbox.entry(&id));
        let count = sandbox.count();
        for destination in [
            format!("new-parent/{}", "x".repeat(300)),
            format!("new-parent/{}/leaf", "x".repeat(300)),
            format!("new-parent/{}", "界".repeat(300)),
            format!("new-parent/{}/leaf", vec!["x".repeat(100); 50].join("/")),
        ] {
            sandbox.cli(
                &[
                    "recovery",
                    "restore",
                    &id,
                    "--to",
                    &destination,
                    "--dry-run",
                ],
                1,
            );
            sandbox.cli(&["recovery", "restore", &id, "--to", &destination], 1);
            assert!(!sandbox.root.join("new-parent").exists());
            assert!(!sandbox.root.join(".overgit/local/mutation.json").exists());
            assert_eq!(sandbox.state(), before);
            assert_eq!(sandbox.count(), count);
            assert_eq!(files(&sandbox.entry(&id)), source);
            sandbox.cli(&["doctor", "--fix"], 0);
        }
        // Use the actual filesystem's answer, not a UTF-8 byte-count assumption.
        for component in ["x".repeat(255), "é".repeat(200)] {
            match fs::symlink_metadata(sandbox.root.join(&component)) {
                Err(error) if error.kind() == std::io::ErrorKind::InvalidFilename => continue,
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
                Ok(_) => {}
            }
            let destination = format!("valid-parent/{component}");
            sandbox.cli(
                &[
                    "recovery",
                    "restore",
                    &id,
                    "--to",
                    &destination,
                    "--dry-run",
                ],
                4,
            );
            sandbox.cli(&["recovery", "restore", &id, "--to", &destination], 0);
            assert_eq!(
                fs::read(sandbox.root.join(destination)).unwrap(),
                b"saved\n"
            );
        }
    }

    #[test]
    fn old_absent_invalid_target_journal_can_recover_without_discarding_existing_data() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let sandbox = Sandbox::new();
        let id = sandbox.backup("f", b"saved\n", b"current\n");
        let before = sandbox.state();
        let source = files(&sandbox.entry(&id));
        let snapshot =
            |bytes: &[u8]| serde_json::json!({"mode":"100644","data":STANDARD.encode(bytes)});
        let manifest = fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap();
        let index = fs::read(sandbox.root.join(".overgit/.git/index")).unwrap();
        let exclude = fs::read(sandbox.root.join(".git/info/exclude")).unwrap();
        let head = String::from_utf8(sandbox.cli(&["git", "rev-parse", "HEAD"], 0).stdout).unwrap();
        let destination = format!("old-parent/{}", "x".repeat(300));
        let mut state = serde_json::json!({
            "version":1,"head":head.trim(),"index":snapshot(&index),"shared_indexes":{},
            "indexed":{"f":snapshot(b"current\n"),".overgit/manifest.json":snapshot(&manifest)},
            "manifest":snapshot(&manifest),"exclude":snapshot(&exclude),"detached":null,
            "work":{destination.clone():snapshot(b"must not discard original")},"flags":{destination.clone():null}
        });
        fs::create_dir(sandbox.root.join("old-parent")).unwrap();
        fs::write(
            sandbox.root.join("old-parent/unrelated"),
            b"retain directory content\n",
        )
        .unwrap();
        let journal = sandbox.root.join(".overgit/local/mutation.json");
        fs::write(&journal, serde_json::to_vec(&state).unwrap()).unwrap();
        sandbox.cli(&["doctor", "--fix"], 1);
        assert!(journal.exists());
        state["work"][&destination] = Value::Null;
        fs::write(&journal, serde_json::to_vec(&state).unwrap()).unwrap();
        sandbox.cli(&["doctor", "--fix"], 0);
        assert!(!journal.exists());
        assert_eq!(sandbox.state(), before);
        assert_eq!(files(&sandbox.entry(&id)), source);
        assert_eq!(
            fs::read(sandbox.root.join("old-parent/unrelated")).unwrap(),
            b"retain directory content\n"
        );
    }
}
