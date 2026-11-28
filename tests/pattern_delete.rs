#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{Command, Output},
    };

    fn isolated(mut command: Command) -> Command {
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(key);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Pattern Test")
            .env("GIT_AUTHOR_EMAIL", "pattern@example.invalid")
            .env("GIT_COMMITTER_NAME", "Pattern Test")
            .env("GIT_COMMITTER_EMAIL", "pattern@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn git_ok(dir: &Path, args: &[&str]) -> Vec<u8> {
        let output = isolated(Command::new("git"))
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn cli_code(dir: &Path, args: &[&str], expected: i32) -> Output {
        let output = isolated(Command::new(env!("CARGO_BIN_EXE_overgit")))
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "overgit {:?} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, bytes).unwrap();
    }

    fn manifest(root: &Path) -> Value {
        serde_json::from_slice(&fs::read(root.join(".overgit/manifest.json")).unwrap()).unwrap()
    }

    fn backup_contents(dir: &Path) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(backup_contents(&path));
            } else if path.file_name().is_some_and(|name| name == "content") {
                found.push(fs::read(path).unwrap());
            }
        }
        found
    }

    struct Sandbox {
        temporary: tempfile::TempDir,
        root: PathBuf,
    }

    impl Sandbox {
        fn new(files: &[(&str, &[u8])]) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("repo");
            fs::create_dir(&root).unwrap();
            git_ok(&root, &["init", "--initial-branch=main"]);
            git_ok(&root, &["config", "user.name", "Test User"]);
            git_ok(&root, &["config", "user.email", "test@example.com"]);
            for (path, bytes) in files {
                write(&root, path, bytes);
            }
            git_ok(&root, &["add", "--all"]);
            git_ok(&root, &["commit", "--allow-empty", "-m", "initial base"]);
            Self { temporary, root }
        }

        fn init(&self) {
            self.run(&["init"]);
            self.run(&["git", "config", "user.name", "Test User"]);
            self.run(&["git", "config", "user.email", "test@example.com"]);
        }

        fn run(&self, args: &[&str]) -> Output {
            cli_code(&self.root, args, 0)
        }

        fn reject(&self, args: &[&str]) -> Output {
            cli_code(&self.root, args, 1)
        }

        fn clean_base(&self) {
            let status = git_ok(
                &self.root,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            );
            assert!(
                status.is_empty(),
                "base repository is dirty:\n{}",
                String::from_utf8_lossy(&status)
            );
        }

        fn upstream(&self, changes: &[(&str, Option<&[u8]>)]) {
            self.run(&["detach"]);
            for (path, bytes) in changes {
                if let Some(bytes) = bytes {
                    write(&self.root, path, bytes);
                } else {
                    let full = self.root.join(path);
                    if full.exists() {
                        fs::remove_file(full).unwrap();
                    }
                }
            }
            git_ok(&self.root, &["add", "--all"]);
            git_ok(&self.root, &["commit", "-m", "upstream"]);
            self.run(&["attach"]);
        }
    }

    #[test]
    fn pattern_rm_hides_matching_files_at_any_depth_and_restores() {
        let sandbox = Sandbox::new(&[
            ("SPEC.md", b"root spec\n"),
            ("a/SPEC.md", b"nested spec\n"),
            ("a/b/c/SPEC.md", b"deep spec\n"),
            ("docs/README.md", b"keep readme\n"),
            ("SPEC.md_other", b"keep similar name\n"),
        ]);
        let root = &sandbox.root;
        sandbox.init();
        sandbox.run(&["rm", "a/SPEC.md"]);
        write(root, "untracked/SPEC.md", b"untracked spec\n");

        // Run from a subdirectory to prove patterns are repository-wide.
        cli_code(&root.join("a"), &["rm", "--pattern", "SPEC.md"], 0);
        assert_eq!(manifest(root)["patterns"], json!(["SPEC.md"]));
        assert_eq!(manifest(root)["entries"], json!({}));
        assert!(!root.join("SPEC.md").exists());
        assert!(!root.join("a/SPEC.md").exists());
        assert!(!root.join("a/b/c/SPEC.md").exists());
        assert_eq!(
            fs::read(root.join("docs/README.md")).unwrap(),
            b"keep readme\n"
        );
        assert_eq!(
            fs::read(root.join("SPEC.md_other")).unwrap(),
            b"keep similar name\n"
        );
        assert_eq!(
            fs::read(root.join("untracked/SPEC.md")).unwrap(),
            b"untracked spec\n"
        );

        assert_eq!(
            sandbox
                .run(&["list", "--kind", "pattern", "--porcelain"])
                .stdout,
            b"pattern\tSPEC.md\0"
        );
        let which = cli_code(
            root,
            &[
                "which",
                "--porcelain",
                "a/SPEC.md",
                "docs/README.md",
                "untracked/SPEC.md",
            ],
            1,
        );
        assert_eq!(
        which.stdout,
        b"overlay\tpattern\ta/SPEC.md\0base\ttracked\tdocs/README.md\0base\tuntracked\tuntracked/SPEC.md\0"
    );

        let status: Value =
            serde_json::from_slice(&sandbox.run(&["status", "--porcelain"]).stdout).unwrap();
        assert_eq!(status["patterns"], json!(["SPEC.md"]));
        let doctor: Value =
            serde_json::from_slice(&sandbox.run(&["doctor", "--porcelain"]).stdout).unwrap();
        assert!(
            doctor
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["code"] == "delete-pattern" && item["severity"] == "info")
        );

        // Explicit replacement overrides the pattern rule.
        write(root, "a/SPEC.md", b"custom spec\n");
        sandbox.run(&["add", "a/SPEC.md"]);
        sandbox.run(&["commit", "-m", "hide specs with one replacement"]);
        assert_eq!(
            cli_code(root, &["which", "--porcelain", "a/SPEC.md"], 0).stdout,
            b"overlay\toverride\ta/SPEC.md\0"
        );

        sandbox.run(&["detach"]);
        assert_eq!(fs::read(root.join("SPEC.md")).unwrap(), b"root spec\n");
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"nested spec\n");
        assert_eq!(
            fs::read(root.join("a/b/c/SPEC.md")).unwrap(),
            b"deep spec\n"
        );
        sandbox.run(&["attach"]);
        assert!(!root.join("SPEC.md").exists());
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"custom spec\n");
        assert!(!root.join("a/b/c/SPEC.md").exists());

        // Restoring a single replacement leaves the pattern active and re-hides the base file.
        sandbox.run(&["restore", "a/SPEC.md"]);
        assert!(!root.join("a/SPEC.md").exists());
        let err = sandbox.reject(&["restore", "a/SPEC.md"]);
        assert!(String::from_utf8_lossy(&err.stderr).contains("--pattern"));

        // Restoring the pattern keeps explicit overrides intact while restoring pattern-hidden files.
        write(root, "a/SPEC.md", b"custom spec\n");
        sandbox.run(&["add", "a/SPEC.md"]);
        sandbox.run(&["restore", "--pattern", "SPEC.md"]);
        assert!(manifest(root).get("patterns").is_none());
        assert_eq!(fs::read(root.join("SPEC.md")).unwrap(), b"root spec\n");
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"custom spec\n");
        assert_eq!(
            fs::read(root.join("a/b/c/SPEC.md")).unwrap(),
            b"deep spec\n"
        );

        sandbox.run(&["restore", "--all"]);
        fs::remove_dir_all(root.join("untracked")).unwrap();
        sandbox.clean_base();
    }

    #[test]
    fn pattern_syntax_covers_globs_anchors_doublestar_and_rejects_invalid() {
        let sandbox = Sandbox::new(&[
            ("SPEC.md", b"root\n"),
            ("sub/SPEC.md", b"sub\n"),
            ("docs/a.md", b"docs a\n"),
            ("docs/nested/b.md", b"docs b\n"),
            ("other/docs/c.md", b"other docs\n"),
            ("dir", b"file named dir\n"),
            ("tree/one", b"tree one\n"),
            ("tree/sub/two", b"tree two\n"),
            ("item1.txt", b"one\n"),
            ("itema.txt", b"alpha\n"),
            ("star*file", b"literal star\n"),
        ]);
        let root = &sandbox.root;
        sandbox.init();

        // Anchored pattern only hides root SPEC.md.
        sandbox.run(&["rm", "--pattern", "/SPEC.md"]);
        assert!(!root.join("SPEC.md").exists());
        assert!(root.join("sub/SPEC.md").exists());
        sandbox.run(&["restore", "--pattern", "/SPEC.md"]);
        assert!(root.join("SPEC.md").exists());

        // Slash without ** matches only direct children under docs/.
        sandbox.run(&["rm", "--pattern", "docs/*.md"]);
        assert!(!root.join("docs/a.md").exists());
        assert!(root.join("docs/nested/b.md").exists());
        assert!(root.join("other/docs/c.md").exists());
        sandbox.run(&["restore", "--pattern", "docs/*.md"]);

        // ** in middle matches zero or more directories.
        sandbox.run(&["rm", "--pattern", "docs/**/*.md"]);
        assert!(!root.join("docs/a.md").exists());
        assert!(!root.join("docs/nested/b.md").exists());
        assert!(root.join("other/docs/c.md").exists());
        sandbox.run(&["restore", "--pattern", "docs/**/*.md"]);

        // Trailing ** requires at least one segment beneath the prefix (does not match file "dir").
        sandbox.run(&["rm", "--pattern", "dir/**", "--pattern", "tree/**"]);
        assert!(root.join("dir").exists());
        assert!(!root.join("tree/one").exists());
        assert!(!root.join("tree/sub/two").exists());
        sandbox.run(&["restore", "--all"]);

        // Character classes, ?, and backslash escapes.
        sandbox.run(&[
            "rm",
            "--pattern",
            "item[0-9].???",
            "--pattern",
            r"star\*file",
        ]);
        assert!(!root.join("item1.txt").exists());
        assert!(root.join("itema.txt").exists());
        assert!(!root.join("star*file").exists());
        sandbox.run(&["restore", "--all"]);

        // Proactive rule with zero current matches succeeds and hides future upstream files.
        sandbox.run(&["rm", "--pattern", "FUTURE.md"]);
        assert_eq!(manifest(root)["patterns"], json!(["FUTURE.md"]));
        sandbox.upstream(&[("pkg/FUTURE.md", Some(b"later\n"))]);
        assert!(!root.join("pkg/FUTURE.md").exists());
        sandbox.run(&["restore", "--pattern", "FUTURE.md"]);
        assert!(root.join("pkg/FUTURE.md").exists());

        // Invalid patterns are rejected without mutating manifest.
        for invalid in [
            "",
            "/",
            "!SPEC.md",
            "docs/",
            "../SPEC.md",
            "./SPEC.md",
            "a//b",
            ".git",
            ".overgit",
            "sub/.git",
            r"\.git",
            r"sub/\.git",
            r"\.overgit",
            r"/\.overgit",
            r"\.",
            r"\.\.",
            r"a/\.\./b",
            "[unclosed",
            "[z-a]",
            "trail\\",
        ] {
            sandbox.reject(&["rm", "--pattern", invalid]);
        }
        assert!(manifest(root).get("patterns").is_none());
        sandbox.clean_base();
    }

    #[test]
    fn pattern_sync_and_base_pull_hide_new_upstream_files() {
        let sandbox = Sandbox::new(&[("keep.txt", b"keep\n")]);
        let root = &sandbox.root;
        let remote = sandbox.temporary.path().join("base-remote.git");
        git_ok(
            sandbox.temporary.path(),
            &[
                "clone",
                "--bare",
                root.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git_ok(root, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git_ok(root, &["fetch", "origin"]);
        git_ok(root, &["branch", "--set-upstream-to=origin/main", "main"]);

        sandbox.init();
        sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        sandbox.run(&["commit", "-m", "hide specs"]);

        let writer = sandbox.temporary.path().join("writer");
        git_ok(
            sandbox.temporary.path(),
            &["clone", remote.to_str().unwrap(), writer.to_str().unwrap()],
        );
        git_ok(&writer, &["config", "user.name", "Upstream"]);
        git_ok(&writer, &["config", "user.email", "up@example.com"]);
        write(&writer, "pkg/nested/SPEC.md", b"upstream spec\n");
        write(&writer, "pkg/code.rs", b"fn main() {}\n");
        git_ok(&writer, &["add", "--all"]);
        git_ok(&writer, &["commit", "-m", "add spec and code"]);
        git_ok(&writer, &["push"]);

        // Pull via `overgit base pull` automatically hides new pattern matches.
        sandbox.run(&["base", "pull"]);
        assert!(!root.join("pkg/nested/SPEC.md").exists());
        assert_eq!(
            fs::read(root.join("pkg/code.rs")).unwrap(),
            b"fn main() {}\n"
        );
        sandbox.clean_base();
        sandbox.run(&["doctor"]);

        // Also test a raw git pull while attached followed by `overgit sync`.
        write(&writer, "other/SPEC.md", b"second spec\n");
        git_ok(&writer, &["add", "--all"]);
        git_ok(&writer, &["commit", "-m", "second spec"]);
        git_ok(&writer, &["push"]);
        git_ok(root, &["pull", "--ff-only"]);
        assert!(root.join("other/SPEC.md").exists());
        cli_code(root, &["sync", "--dry-run"], 4);
        sandbox.run(&["sync"]);
        assert!(!root.join("other/SPEC.md").exists());
        sandbox.clean_base();
        sandbox.run(&["doctor"]);
    }

    #[test]
    fn pattern_safety_preserves_uncommitted_edits_and_staged_base_changes() {
        let sandbox = Sandbox::new(&[("a/SPEC.md", b"base a\n"), ("b/SPEC.md", b"base b\n")]);
        let root = &sandbox.root;
        sandbox.init();

        // Uncommitted local edit blocks `rm --pattern` without --force, leaving everything untouched.
        write(root, "a/SPEC.md", b"local edit\n");
        sandbox.reject(&["rm", "--pattern", "SPEC.md"]);
        assert!(root.join("a/SPEC.md").exists());
        assert!(root.join("b/SPEC.md").exists());
        assert!(manifest(root).get("patterns").is_none());

        // With --force, uncommitted edit is backed up before hiding.
        sandbox.run(&["rm", "--force", "--pattern", "SPEC.md"]);
        assert!(!root.join("a/SPEC.md").exists());
        assert!(!root.join("b/SPEC.md").exists());
        assert!(
            backup_contents(&root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"local edit\n")
        );

        // Unexpected working content at a hidden path blocks `restore --pattern` without --force.
        write(root, "b/SPEC.md", b"unexpected work\n");
        sandbox.reject(&["restore", "--pattern", "SPEC.md"]);
        assert_eq!(
            fs::read(root.join("b/SPEC.md")).unwrap(),
            b"unexpected work\n"
        );
        sandbox.run(&["restore", "--force", "--pattern", "SPEC.md"]);
        assert_eq!(fs::read(root.join("b/SPEC.md")).unwrap(), b"base b\n");
        assert!(
            backup_contents(&root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"unexpected work\n")
        );

        // Staged base change blocks `rm --pattern`.
        write(root, "a/SPEC.md", b"staged change\n");
        git_ok(root, &["add", "a/SPEC.md"]);
        sandbox.reject(&["rm", "--pattern", "SPEC.md"]);
        git_ok(root, &["reset", "--hard", "HEAD"]);

        // Staged base change at a newly matched path blocks sync/apply and is reported by doctor.
        sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        write(root, "c/SPEC.md", b"staged new spec\n");
        git_ok(root, &["add", "c/SPEC.md"]);
        sandbox.reject(&["sync"]);
        sandbox.reject(&["apply"]);
        let doctor = cli_code(root, &["doctor", "--porcelain"], 4);
        let problems: Value = serde_json::from_slice(&doctor.stdout).unwrap();
        assert!(
            problems
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["code"] == "base-index-divergence" && p["path"] == "c/SPEC.md")
        );
    }

    #[test]
    fn pattern_doctor_hides_visible_file_and_rescues_edits() {
        let sandbox = Sandbox::new(&[("a/SPEC.md", b"base a\n")]);
        let root = &sandbox.root;
        sandbox.init();
        sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        sandbox.run(&["commit", "-m", "hide specs"]);

        // Simulate drift where the hidden file reappears with local edits.
        write(root, "a/SPEC.md", b"drifted edit\n");
        let doctor = cli_code(root, &["doctor", "--porcelain"], 4);
        let problems: Value = serde_json::from_slice(&doctor.stdout).unwrap();
        assert!(
            problems
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["code"] == "pattern-file-visible" && p["path"] == "a/SPEC.md")
        );

        sandbox.run(&["doctor", "--fix"]);
        assert!(!root.join("a/SPEC.md").exists());
        assert!(
            backup_contents(&root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"drifted edit\n")
        );
        sandbox.clean_base();
    }

    #[test]
    fn pattern_overlapping_masks_resolve_drop_keep_file_and_recovery() {
        let sandbox = Sandbox::new(&[
            ("a/SPEC.md", b"a spec\n"),
            ("docs/SPEC.md", b"docs spec\n"),
            ("docs/guide.md", b"guide\n"),
        ]);
        let root = &sandbox.root;
        sandbox.init();
        sandbox.run(&["rm", "docs"]);
        sandbox.run(&["rm", "--pattern", "SPEC.md", "--pattern", "*.md"]);

        // Restoring SPEC.md keeps a/SPEC.md hidden under *.md and docs/SPEC.md hidden under docs.
        sandbox.run(&["restore", "--pattern", "SPEC.md"]);
        assert!(!root.join("a/SPEC.md").exists());
        assert!(!root.join("docs/SPEC.md").exists());

        // Restoring *.md exposes a/SPEC.md while keeping docs/SPEC.md hidden under opaque docs.
        sandbox.run(&["restore", "--pattern", "*.md"]);
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"a spec\n");
        assert!(!root.join("docs/SPEC.md").exists());

        // Dropping an explicit addition collision under an active pattern hides the upstream file.
        sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        write(root, "new/SPEC.md", b"my addition\n");
        sandbox.run(&["add", "new/SPEC.md"]);
        sandbox.run(&["commit", "-m", "add custom spec"]);
        sandbox.upstream(&[("new/SPEC.md", Some(b"upstream spec\n"))]);
        cli_code(root, &["sync"], 3);
        sandbox.run(&["resolve", "--drop", "new/SPEC.md"]);
        assert!(!root.join("new/SPEC.md").exists());
        sandbox.clean_base();

        // Recovery restore refuses to write directly onto a pattern-masked whiteout.
        let list: Value =
            serde_json::from_slice(&sandbox.run(&["recovery", "list", "--porcelain"]).stdout)
                .unwrap();
        let id = list["entries"][0]["id"].as_str().unwrap();
        sandbox.reject(&["recovery", "restore", id]);

        // restore --keep-file preserves an untracked Add matching a pattern, and hides if tracked.
        write(root, "private/SPEC.md", b"private\n");
        sandbox.run(&["add", "private/SPEC.md"]);
        sandbox.run(&["restore", "--keep-file", "private/SPEC.md"]);
        assert_eq!(
            fs::read(root.join("private/SPEC.md")).unwrap(),
            b"private\n"
        );
        fs::remove_dir_all(root.join("private")).unwrap();

        write(root, "collide/SPEC.md", b"personal\n");
        sandbox.run(&["add", "collide/SPEC.md"]);
        sandbox.upstream(&[("collide/SPEC.md", Some(b"upstream\n"))]);
        sandbox.run(&["restore", "--keep-file", "collide/SPEC.md"]);
        assert!(!root.join("collide/SPEC.md").exists());
        assert!(
            backup_contents(&root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"personal\n")
        );
        sandbox.clean_base();
    }

    #[test]
    fn pattern_manifest_rejects_invalid_and_redundant_whiteouts() {
        let sandbox = Sandbox::new(&[("a/SPEC.md", b"base\n")]);
        let root = &sandbox.root;
        sandbox.init();
        for invalid in [
            json!({"version":2,"entries":{},"patterns":["!SPEC.md"]}),
            json!({"version":2,"entries":{},"patterns":["docs/"]}),
            json!({"version":2,"entries":{},"patterns":["[unclosed"]}),
            json!({
                "version": 2,
                "entries": {
                    "a/SPEC.md": {"kind": "delete", "base": {"mode": "100644", "data": "YmFzZQo="}}
                },
                "patterns": ["SPEC.md"]
            }),
        ] {
            write(
                root,
                ".overgit/manifest.json",
                &serde_json::to_vec_pretty(&invalid).unwrap(),
            );
            sandbox.reject(&["list"]);
            sandbox.reject(&["apply"]);
        }
    }

    #[test]
    fn overlay_pull_installs_and_releases_patterns() {
        let sandbox = Sandbox::new(&[("a/SPEC.md", b"a\n"), ("b/SPEC.md", b"b\n")]);
        let root = &sandbox.root;
        let remote = sandbox.temporary.path().join("overlay.git");
        git_ok(
            sandbox.temporary.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        cli_code(root, &["init", "--remote", remote.to_str().unwrap()], 0);
        sandbox.run(&["git", "config", "user.name", "Test User"]);
        sandbox.run(&["git", "config", "user.email", "test@example.com"]);
        sandbox.run(&["push", "-u", "origin", "main"]);

        let other = sandbox.temporary.path().join("other");
        git_ok(
            sandbox.temporary.path(),
            &["clone", root.to_str().unwrap(), other.to_str().unwrap()],
        );
        cli_code(&other, &["clone", remote.to_str().unwrap()], 0);
        cli_code(&other, &["git", "config", "user.name", "Other"], 0);
        cli_code(&other, &["git", "config", "user.email", "o@example.com"], 0);
        cli_code(&other, &["rm", "--pattern", "SPEC.md"], 0);
        cli_code(&other, &["commit", "-m", "add spec pattern"], 0);
        cli_code(&other, &["push"], 0);

        sandbox.run(&["pull"]);
        assert!(!root.join("a/SPEC.md").exists());
        assert!(!root.join("b/SPEC.md").exists());
        assert_eq!(manifest(root)["patterns"], json!(["SPEC.md"]));
        sandbox.clean_base();

        cli_code(&other, &["restore", "--pattern", "SPEC.md"], 0);
        cli_code(&other, &["commit", "-m", "remove spec pattern"], 0);
        cli_code(&other, &["push"], 0);

        sandbox.run(&["pull"]);
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"a\n");
        assert_eq!(fs::read(root.join("b/SPEC.md")).unwrap(), b"b\n");
        assert!(manifest(root).get("patterns").is_none());
        sandbox.clean_base();
    }

    #[test]
    fn pattern_rm_preserves_explicit_add_and_override_and_is_idempotent() {
        let sandbox = Sandbox::new(&[("a/SPEC.md", b"base a\n"), ("b/SPEC.md", b"base b\n")]);
        let root = &sandbox.root;
        sandbox.init();
        write(root, "a/SPEC.md", b"my override\n");
        write(root, "notes/SPEC.md", b"my addition\n");
        sandbox.run(&["add", "a/SPEC.md", "notes/SPEC.md"]);
        sandbox.run(&["commit", "-m", "own two specs"]);

        sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"my override\n");
        assert_eq!(
            fs::read(root.join("notes/SPEC.md")).unwrap(),
            b"my addition\n"
        );
        assert!(!root.join("b/SPEC.md").exists());

        // Repeating `rm --pattern` with an existing rule is a no-op and preserves replacements.
        let before = fs::read(root.join(".overgit/manifest.json")).unwrap();
        let again = sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        assert!(again.stdout.is_empty());
        assert_eq!(
            fs::read(root.join(".overgit/manifest.json")).unwrap(),
            before
        );
        assert_eq!(fs::read(root.join("a/SPEC.md")).unwrap(), b"my override\n");
        assert_eq!(
            fs::read(root.join("notes/SPEC.md")).unwrap(),
            b"my addition\n"
        );
        sandbox.clean_base();
    }

    #[test]
    fn staged_base_deletion_blocks_pattern_rm_and_pull() {
        let sandbox = Sandbox::new(&[("a/SPEC.md", b"base a\n"), ("keep.txt", b"keep\n")]);
        let root = &sandbox.root;
        let remote = sandbox.temporary.path().join("overlay.git");
        git_ok(
            sandbox.temporary.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        cli_code(root, &["init", "--remote", remote.to_str().unwrap()], 0);
        sandbox.run(&["git", "config", "user.name", "Test User"]);
        sandbox.run(&["git", "config", "user.email", "test@example.com"]);
        sandbox.run(&["push", "-u", "origin", "main"]);

        let other = sandbox.temporary.path().join("other");
        git_ok(
            sandbox.temporary.path(),
            &["clone", root.to_str().unwrap(), other.to_str().unwrap()],
        );
        cli_code(&other, &["clone", remote.to_str().unwrap()], 0);
        cli_code(&other, &["git", "config", "user.name", "Other"], 0);
        cli_code(&other, &["git", "config", "user.email", "o@example.com"], 0);
        cli_code(&other, &["rm", "--pattern", "SPEC.md"], 0);
        cli_code(&other, &["commit", "-m", "add pattern"], 0);
        cli_code(&other, &["push"], 0);

        git_ok(root, &["rm", "a/SPEC.md"]);
        sandbox.reject(&["rm", "--pattern", "SPEC.md"]);
        assert!(manifest(root).get("patterns").is_none());
        sandbox.reject(&["pull"]);
        assert!(manifest(root).get("patterns").is_none());
    }

    #[test]
    fn pattern_handles_symlinks_rejects_submodules_and_preserves_case_variant_files() {
        let sandbox = Sandbox::new(&[("target.txt", b"target\n")]);
        let root = &sandbox.root;
        std::os::unix::fs::symlink("target.txt", root.join("SPEC.md")).unwrap();
        git_ok(root, &["add", "SPEC.md"]);
        git_ok(root, &["commit", "-m", "add symlink spec"]);

        let sub_source = sandbox.temporary.path().join("sub-source");
        fs::create_dir(&sub_source).unwrap();
        git_ok(&sub_source, &["init", "--initial-branch=main"]);
        git_ok(&sub_source, &["config", "user.name", "Test User"]);
        git_ok(&sub_source, &["config", "user.email", "test@example.com"]);
        write(&sub_source, "file", b"sub\n");
        git_ok(&sub_source, &["add", "--all"]);
        git_ok(&sub_source, &["commit", "-m", "sub"]);
        git_ok(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_source.to_str().unwrap(),
                "vendor/mod",
            ],
        );
        git_ok(root, &["commit", "-m", "add submodule"]);

        sandbox.init();
        // Submodule match is rejected without touching it.
        sandbox.reject(&["rm", "--pattern", "mod"]);
        assert!(root.join("vendor/mod/file").exists());

        // Symlink leaf is hidden without modifying its target, and restored as a symlink.
        sandbox.run(&["rm", "--pattern", "SPEC.md"]);
        assert!(!root.join("SPEC.md").exists());
        assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"target\n");

        // On case-insensitive filesystems, an untracked case-variant file is not deleted by doctor --fix.
        write(root, "probe_CASE", b"1");
        let case_insensitive = root.join("probe_case").exists();
        fs::remove_file(root.join("probe_CASE")).unwrap();
        if case_insensitive {
            write(root, "spec.md", b"untracked lowercase\n");
            let doctor = cli_code(root, &["doctor", "--porcelain"], 4);
            let problems: Value = serde_json::from_slice(&doctor.stdout).unwrap();
            assert!(
                problems
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["code"] == "path-case-mismatch" && p["path"] == "SPEC.md")
            );
            cli_code(root, &["doctor", "--fix"], 4);
            assert_eq!(
                fs::read(root.join("spec.md")).unwrap(),
                b"untracked lowercase\n"
            );
            fs::remove_file(root.join("spec.md")).unwrap();
        }

        sandbox.run(&["restore", "--pattern", "SPEC.md"]);
        assert!(
            fs::symlink_metadata(root.join("SPEC.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        sandbox.clean_base();
    }
}
