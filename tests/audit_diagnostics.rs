#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use serde_json::Value;
    use std::{
        fs,
        io::Write as _,
        path::PathBuf,
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
                .env("GIT_AUTHOR_NAME", "Audit Diagnostics")
                .env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
                .env("GIT_COMMITTER_NAME", "Audit Diagnostics")
                .env("GIT_COMMITTER_EMAIL", "audit@example.invalid");
            command
        }
        fn git(&self, args: &[&str]) -> Vec<u8> {
            let output = self.command("git").args(args).output().unwrap();
            assert!(
                output.status.success(),
                "Git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }
        fn git_input(&self, args: &[&str], input: &[u8]) {
            let mut child = self
                .command("git")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "Git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
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
                "overgit {args:?}\n{}\n{}",
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
        fn own(&self) {
            self.write("f", b"saved overlay\n");
            self.cli(&["add", "f"], 0);
            self.cli(&["commit", "-m", "overlay"], 0);
        }
        fn doctor(&self, expected: i32) -> Value {
            let report: Value =
                serde_json::from_slice(&self.cli(&["doctor", "--porcelain"], expected).stdout)
                    .unwrap();
            for problem in report.as_array().unwrap() {
                assert!(problem["code"].is_string());
                assert!(problem["path"].is_null() || problem["path"].is_string());
                assert!(problem["fixable"].is_boolean());
                assert!(problem["message"].is_string());
                assert!(problem["hint"].is_string());
            }
            report
        }
        fn metadata(&self) -> Vec<Vec<u8>> {
            [
                ".overgit/manifest.json",
                ".overgit/.git/index",
                ".git/index",
                ".git/info/exclude",
            ]
            .iter()
            .map(|path| fs::read(self.root.join(path)).unwrap())
            .collect()
        }
    }
    fn has_code(report: &Value, code: &str) -> bool {
        report
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == code)
    }

    #[test]
    fn recovery_inventory_is_quiet_when_missing_or_empty() {
        let sandbox = Sandbox::new();
        assert!(!has_code(&sandbox.doctor(0), "recovery-entries"));
        fs::create_dir_all(sandbox.root.join(".overgit/local/backups")).unwrap();
        assert!(!has_code(&sandbox.doctor(0), "recovery-entries"));
        let status = sandbox.cli(&["status"], 0);
        assert!(
            !String::from_utf8(status.stdout)
                .unwrap()
                .contains("recovery-entries")
        );
    }

    #[test]
    fn actual_recovery_entries_are_informational_without_reading_payloads() {
        let sandbox = Sandbox::new();
        sandbox.own();
        sandbox.write("f", b"secret staged-only recovery bytes\n");
        sandbox.cli(&["add", "--force", "f"], 0);
        sandbox.write("f", b"current overlay\n");
        sandbox.cli(&["add", "--force", "f"], 0);
        sandbox.cli(&["commit", "-m", "current"], 0);
        sandbox.cli(&["git", "gc", "--prune=now"], 0);
        let backups = sandbox.root.join(".overgit/local/backups");
        let rescue = fs::read_dir(&backups)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::read(rescue.join("content")).unwrap(),
            b"secret staged-only recovery bytes\n"
        );
        // Malformed metadata and incomplete archive formats remain visible without
        // being parsed, validated, or mistaken for guaranteed recoverable versions.
        fs::write(rescue.join("recovery.json"), b"malformed metadata\n").unwrap();
        fs::create_dir(backups.join("pull-aborted-incomplete")).unwrap();
        fs::write(backups.join("unrecognized-entry"), b"opaque data\n").unwrap();
        let count = fs::read_dir(&backups).unwrap().count();
        let before = sandbox.metadata();
        let report = sandbox.doctor(0);
        let info = report
            .as_array()
            .unwrap()
            .iter()
            .find(|problem| problem["code"] == "recovery-entries")
            .unwrap();
        assert_eq!(info["severity"], "info");
        assert_eq!(info["path"], ".overgit/local/backups");
        assert_eq!(info["fixable"], false);
        assert!(
            info["message"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{count} local recovery entries"))
        );
        assert!(
            info["message"]
                .as_str()
                .unwrap()
                .contains("not been validated")
        );
        assert!(info["hint"].as_str().unwrap().contains("recovery inspect"));
        let status: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert!(has_code(&status["problems"], "recovery-entries"));
        assert!(status["warnings"].as_array().unwrap().is_empty());
        let text = String::from_utf8(sandbox.cli(&["status"], 0).stdout).unwrap();
        assert!(text.contains("info recovery-entries"));
        assert!(text.contains(".overgit/local/backups"));
        assert!(!text.contains("warning recovery-entries"));
        assert!(!text.contains("secret staged-only"));
        assert_eq!(sandbox.metadata(), before);
        assert_eq!(
            fs::read(rescue.join("recovery.json")).unwrap(),
            b"malformed metadata\n"
        );
    }

    #[test]
    fn recovery_inventory_reports_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;
        let sandbox = Sandbox::new();
        let outside = sandbox.temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("private"), b"outside bytes\n").unwrap();
        let backups = sandbox.root.join(".overgit/local/backups");
        fs::create_dir_all(backups.parent().unwrap()).unwrap();
        symlink(&outside, &backups).unwrap();
        let report = sandbox.doctor(4);
        assert!(has_code(&report, "recovery-storage-unavailable"));
        assert!(!has_code(&report, "recovery-entries"));
        fs::remove_file(&backups).unwrap();
        fs::create_dir(&backups).unwrap();
        symlink(&outside, backups.join("linked-entry")).unwrap();
        let report = sandbox.doctor(4);
        assert!(has_code(&report, "recovery-storage-unsafe"));
        let status: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert!(has_code(&status["problems"], "recovery-storage-unsafe"));
        assert!(
            status["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap()
                    .contains("recovery-storage-unsafe"))
        );
        assert_eq!(
            fs::read(outside.join("private")).unwrap(),
            b"outside bytes\n"
        );
    }

    #[test]
    fn mixed_status_and_doctor_preserve_modes_flags_and_upstream_diagnostics() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let sandbox = Sandbox::new();
        sandbox.write("deleted", b"hidden base\n");
        sandbox.write("ordinary", b"unowned base\n");
        sandbox.git(&["add", "deleted", "ordinary"]);
        sandbox.git(&["commit", "-m", "base paths"]);
        sandbox.write("f", b"saved overlay\n");
        sandbox.write("executable", b"#!/bin/sh\n");
        fs::set_permissions(
            sandbox.root.join("executable"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("ordinary", sandbox.root.join("link")).unwrap();
        sandbox.write("missing", b"retained saved bytes\n");
        sandbox.cli(&["add", "f", "executable", "link", "missing"], 0);
        sandbox.cli(&["rm", "deleted"], 0);
        sandbox.cli(&["commit", "-m", "mixed overlay"], 0);
        sandbox.cli(&["detach"], 0);
        sandbox.write("f", b"new upstream\n");
        sandbox.git(&["add", "f"]);
        sandbox.git(&["commit", "-m", "upstream"]);
        sandbox.cli(&["attach"], 0);
        sandbox.write("f", b"local edit\n");
        fs::remove_file(sandbox.root.join("missing")).unwrap();
        fs::set_permissions(
            sandbox.root.join("executable"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        sandbox.git(&["update-index", "--assume-unchanged", "f"]);
        sandbox.git(&["update-index", "--skip-worktree", "ordinary"]);
        let before = sandbox.metadata();
        let report: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        let files = report["files"].as_array().unwrap();
        let file = |path: &str| files.iter().find(|file| file["path"] == path).unwrap();
        assert_eq!(file("f")["upstream"], "changed");
        assert_eq!(file("f")["dirty"], true);
        assert_eq!(file("missing")["missing"], true);
        assert_eq!(file("missing")["saved"], true);
        assert_eq!(file("executable")["dirty"], true);
        assert_eq!(file("link")["dirty"], false);
        assert_eq!(file("deleted")["missing"], false);
        assert_eq!(file("deleted")["saved"], false);
        let diagnosis = sandbox.doctor(4);
        assert_eq!(report["problems"], diagnosis);
        let codes: Vec<_> = diagnosis
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| (finding["code"].as_str().unwrap(), finding["path"].as_str()))
            .collect();
        assert!(codes.contains(&("assume-unchanged", Some("f"))));
        assert!(codes.contains(&("unowned-skip", Some("ordinary"))));
        assert!(codes.contains(&("worktree-drift", Some("missing"))));
        assert!(codes.contains(&("worktree-drift", Some("executable"))));
        assert_eq!(sandbox.metadata(), before);
        sandbox.cli(&["doctor", "--fix"], 4);
        assert_eq!(
            fs::read(sandbox.root.join("missing")).unwrap(),
            b"retained saved bytes\n"
        );
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"local edit\n");
        assert_eq!(
            fs::metadata(sandbox.root.join("executable"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
    }

    #[test]
    fn diagnostic_reads_keep_path_specific_filters_and_report_missing_objects() {
        let sandbox = Sandbox::new();
        sandbox.write(".gitattributes", b"crlf text eol=crlf\nlf text eol=lf\n");
        sandbox.write("crlf", b"shared base\n");
        sandbox.write("lf", b"shared base\n");
        sandbox.git(&["add", ".gitattributes", "crlf", "lf"]);
        sandbox.git(&["commit", "-m", "filtered base"]);
        sandbox.write("crlf", b"overlay crlf\r\n");
        sandbox.write("lf", b"overlay lf\n");
        sandbox.cli(&["add", "crlf", "lf"], 0);
        sandbox.cli(&["commit", "-m", "filtered overlay"], 0);
        let report: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert!(
            report["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| file["upstream"] == "same")
        );
        assert_eq!(sandbox.doctor(0), serde_json::json!([]));
        sandbox.write("lf", b"unique staged object to remove\n");
        sandbox.cli(&["add", "--force", "lf"], 0);
        let oid = String::from_utf8(sandbox.cli(&["git", "rev-parse", ":lf"], 0).stdout).unwrap();
        let oid = oid.trim();
        fs::remove_file(
            sandbox
                .root
                .join(".overgit/.git/objects")
                .join(&oid[..2])
                .join(&oid[2..]),
        )
        .unwrap();
        sandbox.cli(&["status", "--porcelain"], 1);
        let problems = sandbox.doctor(4);
        assert!(
            problems
                .as_array()
                .unwrap()
                .iter()
                .any(|problem| problem["code"] == "path-unavailable" && problem["path"] == "lf")
        );
    }

    #[test]
    fn staged_base_divergence_is_diagnosed_without_overwriting_staged_content() {
        let sandbox = Sandbox::new();
        sandbox.own();
        sandbox.git(&["update-index", "--no-skip-worktree", "--", "f"]);
        sandbox.write("f", b"staged base edit\n");
        sandbox.git(&["add", "f"]);
        let staged = sandbox.git(&["diff", "--cached", "--binary"]);
        let report = sandbox.doctor(4);
        assert!(has_code(&report, "base-index-divergence"));
        sandbox.cli(&["doctor", "--fix", "--porcelain"], 4);
        assert_eq!(sandbox.git(&["diff", "--cached", "--binary"]), staged);
        assert_eq!(sandbox.git(&["show", ":f"]), b"staged base edit\n");
    }

    #[test]
    fn ownership_transitions_are_diagnosed_as_explicit_decisions() {
        let sandbox = Sandbox::new();
        sandbox.own();
        sandbox.write("x", b"private addition\n");
        sandbox.cli(&["add", "x"], 0);
        sandbox.cli(&["commit", "-m", "addition"], 0);
        sandbox.git(&["update-index", "--no-skip-worktree", "--", "f"]);
        sandbox.git(&["rm", "--force", "f"]);
        sandbox.write("x", b"upstream addition\n");
        sandbox.git(&["add", "--force", "x"]);
        sandbox.git(&["commit", "-m", "base transition"]);
        let before = sandbox.metadata();
        let report = sandbox.doctor(4);
        assert!(has_code(&report, "upstream-deleted"));
        assert!(has_code(&report, "add-collision"));
        assert_eq!(sandbox.metadata(), before);
    }

    #[test]
    fn sparse_and_busy_base_contexts_are_visible_in_doctor() {
        let sandbox = Sandbox::new();
        sandbox.git(&["config", "core.sparseCheckout", "true"]);
        assert!(has_code(&sandbox.doctor(4), "base-sparse-checkout"));
        sandbox.git(&["config", "core.sparseCheckout", "false"]);
        let head = sandbox.git(&["rev-parse", "HEAD"]);
        sandbox.write(".git/MERGE_HEAD", &head);
        let before = sandbox.metadata();
        assert!(has_code(&sandbox.doctor(4), "base-operation-in-progress"));
        sandbox.cli(&["doctor", "--fix", "--porcelain"], 4);
        assert_eq!(sandbox.metadata(), before);
    }

    #[test]
    fn corrupt_sync_is_structured_in_both_doctor_and_status() {
        let sandbox = Sandbox::new();
        sandbox.write(".overgit/local/sync.json", b"{broken");
        let before = sandbox.metadata();
        let doctor = sandbox.doctor(4);
        assert!(has_code(&doctor, "sync-state-invalid"));
        let status: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert!(has_code(&status["problems"], "sync-state-invalid"));
        assert_eq!(sandbox.metadata(), before);
    }

    #[test]
    fn doctor_reports_broken_or_missing_overlay_storage_as_findings() {
        let sandbox = Sandbox::new();
        sandbox.own();
        fs::remove_file(sandbox.root.join(".overgit/.git/HEAD")).unwrap();
        assert!(has_code(&sandbox.doctor(4), "overlay-unavailable"));
        fs::remove_dir_all(sandbox.root.join(".overgit/.git")).unwrap();
        let report = sandbox.doctor(4);
        assert!(has_code(&report, "overlay-unavailable"));
        assert!(has_code(&report, "clean-unprotected"));
        assert_eq!(
            fs::read(sandbox.root.join("f")).unwrap(),
            b"saved overlay\n"
        );
    }

    #[test]
    fn invalid_indexed_manifest_falls_back_to_valid_head_without_losing_edits() {
        let sandbox = Sandbox::new();
        sandbox.own();
        sandbox.write(".overgit/manifest.json", b"corrupt indexed manifest\n");
        sandbox.cli(&["git", "add", "--force", ".overgit/manifest.json"], 0);
        sandbox.write("f", b"ordinary local edit\n");
        assert!(has_code(&sandbox.doctor(4), "manifest-invalid"));
        sandbox.cli(&["doctor", "--fix", "--porcelain"], 0);
        let manifest: Value =
            serde_json::from_slice(&fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["entries"]["f"]["kind"], "override");
        assert_eq!(
            fs::read(sandbox.root.join("f")).unwrap(),
            b"ordinary local edit\n"
        );
        assert!(
            fs::read_dir(sandbox.root.join(".overgit/local/backups"))
                .unwrap()
                .any(
                    |entry| fs::read(entry.unwrap().path().join("content")).unwrap()
                        == b"corrupt indexed manifest\n"
                )
        );
    }

    #[test]
    fn explicit_rebuild_recovers_payloads_when_no_manifest_copy_survives() {
        let sandbox = Sandbox::new();
        sandbox.own();
        sandbox.write("orphan", b"staged orphan payload\n");
        sandbox.cli(&["git", "add", "--force", "orphan"], 0);
        sandbox.cli(&["git", "rm", "--cached", ".overgit/manifest.json"], 0);
        sandbox.cli(&["git", "commit", "-m", "manifest lost"], 0);
        sandbox.write(".overgit/manifest.json", b"invalid working copy\n");
        sandbox.cli(&["doctor", "--fix", "--porcelain"], 4);
        assert_eq!(
            fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap(),
            b"invalid working copy\n"
        );
        sandbox.cli(&["doctor", "--fix", "--rebuild", "--porcelain"], 0);
        let manifest: Value =
            serde_json::from_slice(&fs::read(sandbox.root.join(".overgit/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["entries"]["f"]["kind"], "override");
        assert_eq!(manifest["entries"]["orphan"]["kind"], "add");
        assert_eq!(
            fs::read(sandbox.root.join("orphan")).unwrap(),
            b"staged orphan payload\n"
        );
        assert_eq!(
            sandbox.cli(&["git", "show", ":f"], 0).stdout,
            b"saved overlay\n"
        );
    }

    #[test]
    fn orphan_ownership_requires_explicit_rebuild() {
        let sandbox = Sandbox::new();
        sandbox.write("orphan", b"private orphan\n");
        sandbox.cli(&["git", "add", "--force", "orphan"], 0);
        assert!(has_code(&sandbox.doctor(4), "orphan-overlay-file"));
        sandbox.cli(&["doctor", "--fix", "--porcelain"], 4);
        assert!(sandbox.cli(&["list", "--porcelain"], 0).stdout.is_empty());
        sandbox.cli(&["doctor", "--fix", "--rebuild", "--porcelain"], 0);
        assert_eq!(
            sandbox.cli(&["list", "--porcelain"], 0).stdout,
            b"add\torphan\0"
        );
    }

    #[test]
    fn tracked_managed_ignore_pollution_is_reported_but_plain_comments_are_not() {
        let sandbox = Sandbox::new();
        sandbox.write(".gitignore", b"# overgit begin\n/private\n# overgit end\n");
        sandbox.git(&["add", ".gitignore"]);
        sandbox.git(&["commit", "-m", "managed rules leaked"]);
        assert!(has_code(&sandbox.doctor(4), "tracked-ignore-overgit"));
        sandbox.write(
            ".gitignore",
            b"# overgit users keep personal ignores in Git info/exclude\n",
        );
        sandbox.git(&["add", ".gitignore"]);
        sandbox.git(&["commit", "-m", "ordinary documentation comment"]);
        assert!(!has_code(&sandbox.doctor(0), "tracked-ignore-overgit"));
    }

    #[test]
    fn status_exposes_complete_and_interrupted_detach_details() {
        let sandbox = Sandbox::new();
        sandbox.own();
        let head = String::from_utf8(sandbox.cli(&["git", "rev-parse", "HEAD"], 0).stdout).unwrap();
        sandbox.cli(&["detach"], 0);
        let report: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert_eq!(report["detach"]["complete"], true);
        assert_eq!(report["detach"]["overlay_head"], head.trim());
        assert_eq!(report["detach"]["paths"], serde_json::json!(["f"]));
        assert_eq!(report["detach"]["restored"], serde_json::json!(["f"]));
        assert!(report["detach"]["started_unix_secs"].is_number());
        let mut marker = report["detach"].clone();
        marker["complete"] = false.into();
        sandbox.write(
            ".overgit/local/detached.json",
            &serde_json::to_vec(&marker).unwrap(),
        );
        let report: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        assert_eq!(report["detach"]["complete"], false);
        assert!(has_code(&report["problems"], "detach-interrupted"));
    }

    #[test]
    fn case_mismatch_is_reported_without_renaming_or_overwriting_local_bytes() {
        let sandbox = Sandbox::new();
        sandbox.own();
        fs::rename(sandbox.root.join("f"), sandbox.root.join("intermediate")).unwrap();
        fs::rename(sandbox.root.join("intermediate"), sandbox.root.join("F")).unwrap();
        sandbox.write("F", b"case renamed local edit\n");
        assert!(has_code(&sandbox.doctor(4), "path-case-mismatch"));
        sandbox.cli(&["doctor", "--fix", "--porcelain"], 4);
        assert_eq!(
            fs::read(sandbox.root.join("F")).unwrap(),
            b"case renamed local edit\n"
        );
    }

    #[test]
    fn worktree_list_survives_broken_sibling_but_mutations_refuse_unknown_excludes() {
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
        fs::write(
            sibling.join(".git"),
            b"gitdir: /definitely/not/a/repository\n",
        )
        .unwrap();
        let listing = sandbox.cli(&["worktree", "list"], 0).stdout;
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("current"));
        assert!(listing.contains("sibling"));
        assert!(listing.contains("unavailable"));
        sandbox.write("private", b"not yet owned\n");
        let before = sandbox.metadata();
        sandbox.cli(&["add", "private"], 1);
        assert_eq!(sandbox.metadata(), before);
        assert_eq!(
            fs::read(sandbox.root.join("private")).unwrap(),
            b"not yet owned\n"
        );
    }

    #[test]
    fn unrelated_non_utf8_dirty_base_path_remains_reportable_losslessly() {
        let sandbox = Sandbox::new();
        let oid = sandbox.git(&["rev-parse", "HEAD:f"]);
        let mut record =
            format!("100644 {}\t", std::str::from_utf8(&oid).unwrap().trim()).into_bytes();
        let raw_path = b"non-utf8-\xff";
        record.extend_from_slice(raw_path);
        record.push(0);
        sandbox.git_input(&["update-index", "-z", "--index-info"], &record);
        let before = fs::read(sandbox.root.join(".git/index")).unwrap();
        let report: Value =
            serde_json::from_slice(&sandbox.cli(&["status", "--porcelain"], 0).stdout).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw_path);
        assert!(
            report["base_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path_utf8"] == false && change["path_bytes"] == encoded)
        );
        assert_eq!(fs::read(sandbox.root.join(".git/index")).unwrap(), before);
    }
}
