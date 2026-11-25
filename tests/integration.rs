#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write as _,
        os::unix::fs::{PermissionsExt as _, symlink},
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
    };
    use tempfile::TempDir;

    struct Sandbox {
        temporary: TempDir,
        root: PathBuf,
    }

    fn isolated(command: &mut Command) -> &mut Command {
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(variable);
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Overgit Test")
            .env("GIT_AUTHOR_EMAIL", "overgit@example.invalid")
            .env("GIT_COMMITTER_NAME", "Overgit Test")
            .env("GIT_COMMITTER_EMAIL", "overgit@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat")
    }

    fn git(cwd: &Path, arguments: &[&str]) -> Output {
        isolated(Command::new("git").current_dir(cwd))
            .args(["-c", "protocol.file.allow=always"])
            .args(arguments)
            .output()
            .unwrap()
    }

    fn git_ok(cwd: &Path, arguments: &[&str]) -> Vec<u8> {
        let output = git(cwd, arguments);
        assert!(
            output.status.success(),
            "Git {arguments:?} in {} failed\nstdout {}\nstderr {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn cli(cwd: &Path, arguments: &[&str]) -> Output {
        isolated(Command::new(env!("CARGO_BIN_EXE_overgit")).current_dir(cwd))
            .args(arguments)
            .output()
            .unwrap()
    }

    fn cli_code(cwd: &Path, arguments: &[&str], code: i32) -> Output {
        let output = cli(cwd, arguments);
        assert_eq!(
            output.status.code(),
            Some(code),
            "overgit {arguments:?} in {}\nstdout {}\nstderr {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    impl Sandbox {
        fn new(files: &[(&str, &[u8])]) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("base");
            fs::create_dir(&root).unwrap();
            git_ok(&root, &["init", "--initial-branch=main"]);
            for (name, bytes) in files {
                write(&root, name, bytes);
            }
            git_ok(&root, &["add", "--all"]);
            git_ok(&root, &["commit", "--allow-empty", "-m", "base"]);
            Self { temporary, root }
        }

        fn init(&self) {
            cli_code(&self.root, &["init"], 0);
        }

        fn run(&self, arguments: &[&str]) -> Output {
            cli_code(&self.root, arguments, 0)
        }

        fn reject(&self, arguments: &[&str]) -> Output {
            let output = cli(&self.root, arguments);
            assert!(
                !output.status.success(),
                "unexpected success for {arguments:?}\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            output
        }

        fn clean_base(&self) {
            assert!(
                git_ok(
                    &self.root,
                    &["status", "--porcelain=v1", "--untracked-files=all"]
                )
                .is_empty(),
                "base should be clean"
            );
            git_ok(&self.root, &["add", "--all"]);
            assert!(git_ok(&self.root, &["diff", "--cached", "--name-only"]).is_empty());
        }

        fn overlay(&self, arguments: &[&str]) -> Vec<u8> {
            let mut full = vec!["git"];
            full.extend(arguments);
            self.run(&full).stdout
        }

        fn upstream(&self, edits: &[(&str, Option<&[u8]>)]) {
            self.run(&["detach"]);
            for (name, bytes) in edits {
                if let Some(bytes) = bytes {
                    write(&self.root, name, bytes);
                } else {
                    fs::remove_file(self.root.join(name)).unwrap();
                }
            }
            git_ok(&self.root, &["add", "--all"]);
            git_ok(&self.root, &["commit", "-m", "upstream"]);
            self.run(&["attach"]);
        }
    }

    fn write(root: &Path, name: &str, bytes: &[u8]) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn content(root: &Path, name: &str) -> Vec<u8> {
        fs::read(root.join(name)).unwrap()
    }

    fn backup_contents(directory: &Path) -> Vec<Vec<u8>> {
        if !directory.exists() {
            return Vec::new();
        }
        let mut output = Vec::new();
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                output.extend(backup_contents(&path));
            } else {
                output.push(fs::read(path).unwrap());
            }
        }
        output
    }

    fn manifest(root: &Path) -> serde_json::Value {
        serde_json::from_slice(&content(root, ".overgit/manifest.json")).unwrap()
    }

    #[test]
    fn opaque_manifest_rejects_overlapping_or_unsafe_paths() {
        use serde_json::json;
        let sandbox = Sandbox::new(&[("safe", b"untouched")]);
        sandbox.init();
        let add = json!({"kind":"add","base":null});
        let delete = json!({"kind":"delete","base":{"mode":"100644","data":""}});
        for (opaque, entries) in [
            (json!([""]), json!({})),
            (json!(["."]), json!({})),
            (json!(["a/../b"]), json!({})),
            (json!([".git/info"]), json!({})),
            (json!(["a/"]), json!({})),
            (json!(["rules", "rules/nested"]), json!({})),
            (json!(["Rules", "rules"]), json!({})),
            (json!(["rules"]), json!({"rules":add})),
            (json!(["rules/a"]), json!({"RULES":add})),
            (json!(["rules"]), json!({"Rules/a":add})),
            (json!(["rules"]), json!({"rules/a":delete})),
        ] {
            let invalid = json!({"version":2,"entries":entries,"opaque":opaque});
            write(
                &sandbox.root,
                ".overgit/manifest.json",
                &serde_json::to_vec(&invalid).unwrap(),
            );
            sandbox.reject(&["apply"]);
            assert_eq!(content(&sandbox.root, "safe"), b"untouched");
            assert_eq!(
                git_ok(&sandbox.root, &["ls-files", "-v", "safe"]),
                b"H safe\n"
            );
        }
    }

    #[test]
    fn opaque_directory_squashes_whiteouts_and_mounts_only_the_replacement() {
        let sandbox = Sandbox::new(&[
            (".agents/team/default", b"team"),
            (".agents/old", b"obsolete"),
            (".agents/config", b"base"),
            (".agents-other/keep", b"sibling"),
        ]);
        sandbox.init();
        let root = &sandbox.root;
        sandbox.run(&["rm", ".agents/old", ".agents/team"]);
        assert_eq!(manifest(root)["entries"][".agents/old"]["kind"], "delete");
        sandbox.run(&["rm", ".agents", ".agents/team"]);
        assert_eq!(manifest(root)["opaque"], serde_json::json!([".agents"]));
        assert_eq!(manifest(root)["entries"], serde_json::json!({}));
        assert!(!root.join(".agents").exists());
        assert_eq!(content(root, ".agents-other/keep"), b"sibling");
        assert_eq!(
            sandbox
                .run(&["list", "--kind", "opaque", "--porcelain"])
                .stdout,
            b"opaque\t.agents\0"
        );

        write(root, ".agents/config", b"my config");
        write(root, ".agents/new/tool", b"my tool");
        sandbox.run(&["add", ".agents"]);
        sandbox.clean_base();
        assert_eq!(
            manifest(root)["entries"][".agents/config"]["kind"],
            "override"
        );
        assert_eq!(manifest(root)["entries"][".agents/new/tool"]["kind"], "add");
        let inspect = sandbox.run(&[
            "inspect",
            "--porcelain",
            ".agents",
            ".agents/old",
            ".agents/config",
        ]);
        assert_eq!(inspect.stdout, b"overlay\topaque\t.agents\0overlay\topaque\t.agents/old\0overlay\toverride\t.agents/config\0");
        let status: serde_json::Value =
            serde_json::from_slice(&sandbox.run(&["status", "--porcelain"]).stdout).unwrap();
        assert_eq!(status["opaque"], serde_json::json!([".agents"]));
        sandbox.run(&["commit", "-m", "replace agents"]);

        sandbox.upstream(&[
            (".agents/old", None),
            (".agents/team/default", Some(b"changed")),
            (".agents/team/new", Some(b"upstream")),
        ]);
        assert!(!root.join(".agents/team").exists());
        assert_eq!(content(root, ".agents/config"), b"my config");
        assert_eq!(content(root, ".agents/new/tool"), b"my tool");
        sandbox.run(&["sync"]);
        sandbox.clean_base();

        sandbox.run(&["rm", ".agents/config"]);
        assert!(manifest(root)["entries"].get(".agents/config").is_none());
        assert!(!root.join(".agents/config").exists());
        assert_eq!(
            git_ok(root, &["ls-files", "-v", "--", ".agents/config"]),
            b"S .agents/config\n"
        );
        sandbox.run(&["detach"]);
        assert_eq!(content(root, ".agents/team/default"), b"changed");
        assert_eq!(content(root, ".agents/team/new"), b"upstream");
        assert!(!root.join(".agents/new").exists());
        sandbox.run(&["attach"]);
        sandbox.run(&["restore", ".agents"]);
        assert!(manifest(root).get("opaque").is_none());
        assert_eq!(content(root, ".agents/config"), b"base");
        assert_eq!(content(root, ".agents/team/new"), b"upstream");
        assert!(!root.join(".agents/new").exists());
        sandbox.clean_base();
    }

    #[test]
    fn opaque_replacements_can_switch_files_symlinks_and_directories() {
        let sandbox = Sandbox::new(&[
            ("rules/tool", b"base file"),
            ("rules/skills/nested/item", b"base skill"),
        ]);
        sandbox.init();
        let root = &sandbox.root;
        sandbox.run(&["rm", "rules"]);
        let external = sandbox.temporary.path().join("external");
        write(&external, "nested/item", b"external skill");
        write(root, "rules/tool/new/script", b"personal script");
        symlink(&external, root.join("rules/skills")).unwrap();
        sandbox.run(&["add", "rules"]);
        sandbox.run(&["commit", "-m", "replace rules with a different layout"]);
        sandbox.clean_base();
        let reader = sandbox.temporary.path().join("reader");
        cli_code(
            sandbox.temporary.path(),
            &[
                "clone",
                root.join(".overgit/.git").to_str().unwrap(),
                reader.to_str().unwrap(),
                "--base",
                root.to_str().unwrap(),
            ],
            0,
        );
        assert_eq!(
            content(&reader, "rules/tool/new/script"),
            b"personal script"
        );
        assert_eq!(
            fs::read_link(reader.join("rules/skills")).unwrap(),
            external
        );
        sandbox.run(&["sync"]);
        sandbox.run(&["doctor"]);
        sandbox.run(&["status", "--all"]);
        sandbox.run(&["detach"]);
        assert_eq!(content(root, "rules/tool"), b"base file");
        assert_eq!(content(root, "rules/skills/nested/item"), b"base skill");
        sandbox.run(&["attach"]);
        assert_eq!(fs::read_link(root.join("rules/skills")).unwrap(), external);
        assert_eq!(content(root, "rules/tool/new/script"), b"personal script");
        sandbox.clean_base();

        write(root, "rules/tool/untracked", b"untracked");
        sandbox.reject(&["restore", "rules"]);
        assert_eq!(content(root, "rules/tool/untracked"), b"untracked");
        assert_eq!(content(root, "rules/tool/new/script"), b"personal script");
        assert_eq!(manifest(root)["opaque"], serde_json::json!(["rules"]));
        fs::remove_file(root.join("rules/tool/untracked")).unwrap();

        sandbox.run(&["restore", "rules"]);
        assert_eq!(content(root, "rules/tool"), b"base file");
        assert_eq!(content(root, "rules/skills/nested/item"), b"base skill");
        assert_eq!(content(&external, "nested/item"), b"external skill");
        sandbox.run(&["commit", "-m", "release the directory"]);
        cli_code(&reader, &["pull", "--ff-only"], 0);
        assert_eq!(content(&reader, "rules/tool"), b"base file");
        assert_eq!(content(&reader, "rules/skills/nested/item"), b"base skill");
        assert!(git_ok(&reader, &["status", "--porcelain", "-uall"]).is_empty());
        sandbox.clean_base();
    }

    #[test]
    fn opaque_attach_force_preserves_detached_state_when_obstructed() {
        let sandbox = Sandbox::new(&[("rules/a", b"base a"), ("rules/tool/base", b"base tool")]);
        let root = &sandbox.root;
        sandbox.init();
        sandbox.run(&["rm", "rules"]);
        write(root, "rules/tool", b"overlay tool");
        sandbox.run(&["add", "rules/tool"]);
        sandbox.run(&["commit", "-m", "replace rules"]);
        sandbox.run(&["detach"]);
        fs::remove_file(root.join("rules/a")).unwrap();
        write(root, "rules/a/keep", b"obstructing directory");
        write(root, "rules/tool/local", b"untracked tool data");
        let flags = git_ok(root, &["ls-files", "-v", "-z"]);
        let marker = content(root, ".overgit/local/detached.json");
        let exclude = content(root, ".git/info/exclude");
        let check_failed_attach = || {
            sandbox.reject(&["attach", "--force"]);
            assert_eq!(git_ok(root, &["ls-files", "-v", "-z"]), flags);
            assert_eq!(content(root, "rules/tool/base"), b"base tool");
            assert_eq!(content(root, "rules/tool/local"), b"untracked tool data");
            assert_eq!(content(root, ".overgit/local/detached.json"), marker);
            assert_eq!(content(root, ".git/info/exclude"), exclude);
            assert!(!root.join(".overgit/local/mutation.json").exists());
        };
        check_failed_attach();
        assert_eq!(content(root, "rules/a/keep"), b"obstructing directory");

        let saved = sandbox.temporary.path();
        fs::rename(root.join("rules/a"), saved.join("saved-a")).unwrap();
        check_failed_attach();

        fs::rename(root.join("rules/tool/local"), saved.join("saved-local")).unwrap();
        sandbox.run(&["attach"]);
        assert_eq!(content(root, "rules/tool"), b"overlay tool");
        sandbox.clean_base();
    }

    #[test]
    fn opaque_sync_hides_new_files_from_a_real_base_pull_while_attached() {
        let sandbox = Sandbox::new(&[("rules/old", b"base")]);
        let root = &sandbox.root;
        let writer = sandbox.temporary.path().join("upstream");
        git_ok(root, &["clone", ".", writer.to_str().unwrap()]);
        sandbox.init();
        sandbox.run(&["rm", "rules"]);
        write(root, "rules/custom", b"my rules");
        sandbox.run(&["add", "rules/custom"]);
        write(&writer, "rules/nested/new", b"new upstream rules");
        git_ok(&writer, &["add", "."]);
        git_ok(&writer, &["commit", "-m", "add upstream rules"]);
        git_ok(
            root,
            &["pull", "--ff-only", writer.to_str().unwrap(), "main"],
        );
        assert_eq!(content(root, "rules/nested/new"), b"new upstream rules");
        cli_code(root, &["sync", "--dry-run"], 4);
        sandbox.run(&["sync", "--only", "rules"]);
        assert!(!root.join("rules/nested").exists());
        assert_eq!(content(root, "rules/custom"), b"my rules");
        assert_eq!(
            git_ok(root, &["ls-files", "-v", "--", "rules/nested/new"]),
            b"S rules/nested/new\n"
        );
        assert!(manifest(root)["entries"].get("rules/nested/new").is_none());
        sandbox.run(&["sync", "--dry-run"]);
        sandbox.clean_base();
        sandbox.run(&["restore", "--all"]);
        assert_eq!(content(root, "rules/nested/new"), b"new upstream rules");
        assert!(!root.join("rules/custom").exists());
        sandbox.clean_base();
    }

    #[test]
    fn opaque_sync_and_doctor_preserve_an_uncommitted_base_addition() {
        let sandbox = Sandbox::new(&[("rules/base", b"base")]);
        sandbox.init();
        sandbox.run(&["rm", "rules"]);
        let root = &sandbox.root;
        write(root, "rules/staged", b"staged base bytes");
        git_ok(root, &["add", "rules/staged"]);
        sandbox.reject(&["sync"]);
        sandbox.reject(&["apply"]);
        cli_code(root, &["doctor", "--fix"], 4);
        assert_eq!(content(root, "rules/staged"), b"staged base bytes");
        assert_eq!(
            git_ok(root, &["show", ":rules/staged"]),
            b"staged base bytes"
        );
        assert_eq!(
            git_ok(root, &["ls-files", "-v", "rules/staged"]),
            b"H rules/staged\n"
        );
        git_ok(root, &["commit", "-m", "record base addition"]);
        sandbox.run(&["sync"]);
        assert!(!root.join("rules/staged").exists());
        sandbox.clean_base();
    }

    #[test]
    fn opaque_parent_squashes_an_overlay_only_directory_after_its_last_file_is_gone() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        write(&sandbox.root, "config/nested/local", b"local");
        sandbox.run(&["add", "config/nested"]);
        sandbox.run(&["rm", "config/nested"]);
        assert!(sandbox.root.join("config").is_dir());
        sandbox.run(&["rm", "config"]);
        assert!(!sandbox.root.join("config").exists());
        assert_eq!(
            manifest(&sandbox.root)["opaque"],
            serde_json::json!(["config"])
        );
        sandbox.run(&["restore", "config"]);
        assert!(manifest(&sandbox.root).get("opaque").is_none());
    }

    #[test]
    fn aborting_sync_restores_an_implicitly_hidden_file_and_its_original_base_flags() {
        let sandbox = Sandbox::new(&[("rules/file", b"original\n")]);
        sandbox.init();
        sandbox.run(&["rm", "rules"]);
        write(&sandbox.root, "rules/file", b"personal\n");
        sandbox.run(&["add", "rules/file"]);
        sandbox.upstream(&[
            ("rules/new", Some(b"introduced")),
            ("rules/file", Some(b"upstream\n")),
        ]);
        git_ok(
            &sandbox.root,
            &["update-index", "--no-skip-worktree", "rules/new"],
        );
        git_ok(&sandbox.root, &["checkout", "--", "rules/new"]);
        let flags = git_ok(&sandbox.root, &["ls-files", "-v", "-z"]);
        cli_code(&sandbox.root, &["sync"], 3);
        assert!(!sandbox.root.join("rules/new").exists());
        cli_code(&sandbox.root, &["sync", "--dry-run", "--only", "rules"], 4);
        sandbox.run(&["sync", "--abort"]);
        assert_eq!(content(&sandbox.root, "rules/new"), b"introduced");
        assert_eq!(content(&sandbox.root, "rules/file"), b"personal\n");
        assert_eq!(git_ok(&sandbox.root, &["ls-files", "-v", "-z"]), flags);
    }

    #[test]
    fn opaque_directory_stays_hidden_when_upstream_replaces_it_with_a_file() {
        let sandbox = Sandbox::new(&[("rules/base", b"base")]);
        sandbox.init();
        sandbox.run(&["rm", "rules"]);
        sandbox.run(&["detach"]);
        fs::remove_file(sandbox.root.join("rules/base")).unwrap();
        fs::remove_dir(sandbox.root.join("rules")).unwrap();
        write(&sandbox.root, "rules", b"new upstream file");
        git_ok(&sandbox.root, &["add", "--all"]);
        git_ok(&sandbox.root, &["commit", "-m", "replace base directory"]);
        sandbox.run(&["attach"]);
        sandbox.run(&["sync"]);
        assert!(!sandbox.root.join("rules").exists());
        sandbox.clean_base();
        sandbox.run(&["restore", "rules"]);
        assert_eq!(content(&sandbox.root, "rules"), b"new upstream file");
        sandbox.clean_base();
    }

    #[test]
    fn opaque_attach_and_restore_preserve_a_new_base_file_above_empty_masks() {
        let sandbox = Sandbox::new(&[
            ("assets/icons/icon", b"icon"),
            ("assets/fonts/font", b"font"),
        ]);
        let root = &sandbox.root;
        sandbox.init();
        sandbox.run(&["rm", "assets/icons", "assets/fonts"]);
        sandbox.run(&["detach"]);
        git_ok(root, &["rm", "-r", "assets"]);
        write(root, "assets", b"upstream file");
        git_ok(root, &["add", "assets"]);
        git_ok(root, &["commit", "-m", "replace assets with a file"]);

        sandbox.run(&["attach"]);
        assert_eq!(content(root, "assets"), b"upstream file");
        assert_eq!(git_ok(root, &["ls-files", "-v", "assets"]), b"H assets\n");
        assert_eq!(
            manifest(root)["opaque"],
            serde_json::json!(["assets/fonts", "assets/icons"])
        );

        sandbox.run(&["restore", "assets/icons"]);
        assert_eq!(
            manifest(root)["opaque"],
            serde_json::json!(["assets/fonts"])
        );
        sandbox.run(&["restore", "--all"]);
        assert!(manifest(root).get("opaque").is_none());
        assert_eq!(content(root, "assets"), b"upstream file");
        assert_eq!(git_ok(root, &["ls-files", "-v", "assets"]), b"H assets\n");
        sandbox.clean_base();
    }

    #[test]
    fn opaque_directory_repair_recovers_the_rule_and_rescues_unexpected_work() {
        let sandbox = Sandbox::new(&[("rules/base", b"original")]);
        sandbox.init();
        let root = &sandbox.root;
        write(root, "rules/local", b"untracked");
        write(root, "rules/base", b"unstaged");
        sandbox.reject(&["rm", "rules"]);
        assert!(manifest(root).get("opaque").is_none());
        git_ok(root, &["add", "rules/base"]);
        sandbox.reject(&["rm", "--force", "rules"]);
        git_ok(root, &["reset", "--", "rules/base"]);
        sandbox.run(&["rm", "--force", "rules"]);
        assert_eq!(content(root, "rules/local"), b"untracked");
        assert_eq!(
            sandbox.reject(&["which", "rules/local"]).status.code(),
            Some(1)
        );
        sandbox.run(&["commit", "-m", "mask rules"]);
        write(root, "rules/base", b"unexpected");
        git_ok(root, &["update-index", "--no-skip-worktree", "rules/base"]);
        let problems: Vec<serde_json::Value> =
            serde_json::from_slice(&cli_code(root, &["doctor", "--porcelain"], 4).stdout).unwrap();
        for code in [
            "opaque-directory",
            "opaque-file-visible",
            "owned-skip-missing",
        ] {
            assert!(
                problems.iter().any(|problem| problem["code"] == code),
                "{problems:?}"
            );
        }
        write(root, ".overgit/manifest.json", b"invalid");
        sandbox.run(&["doctor", "--fix", "--rebuild"]);
        assert_eq!(manifest(root)["opaque"], serde_json::json!(["rules"]));
        assert!(!root.join("rules/base").exists());
        assert_eq!(content(root, "rules/local"), b"untracked");
        let backups = backup_contents(&root.join(".overgit/local/backups"));
        for expected in [b"unstaged".as_slice(), b"unexpected"] {
            assert!(backups.iter().any(|bytes| bytes == expected));
        }
        sandbox.run(&["doctor"]);
        assert_eq!(
            git_ok(root, &["status", "--porcelain", "-uall"]),
            b"?? rules/local\n"
        );
    }

    #[test]
    fn dropping_an_explicit_opaque_replacement_keeps_the_new_base_file_hidden() {
        let sandbox = Sandbox::new(&[("rules/base", b"base")]);
        sandbox.init();
        let root = &sandbox.root;
        sandbox.run(&["rm", "rules"]);
        write(root, "rules/added", b"my rules");
        sandbox.run(&["add", "rules/added"]);
        sandbox.upstream(&[("rules/added", Some(b"upstream collision"))]);
        cli_code(root, &["sync"], 3);
        sandbox.run(&["resolve", "--drop", "rules/added"]);
        assert!(!root.join("rules/added").exists());
        assert_eq!(manifest(root)["entries"], serde_json::json!({}));
        sandbox.clean_base();
        sandbox.run(&["restore", "rules"]);
        assert_eq!(content(root, "rules/added"), b"upstream collision");
    }

    #[test]
    fn overlay_pull_installs_and_releases_opaque_directories_against_the_local_base() {
        let sandbox = Sandbox::new(&[("dir/base", b"base")]);
        sandbox.init();
        let reader = sandbox.temporary.path().join("reader");
        let overlay = sandbox.root.join(".overgit/.git");
        cli_code(
            sandbox.temporary.path(),
            &[
                "clone",
                overlay.to_str().unwrap(),
                reader.to_str().unwrap(),
                "--base",
                sandbox.root.to_str().unwrap(),
            ],
            0,
        );
        write(&reader, "dir/client-only", b"local base");
        git_ok(&reader, &["add", "dir/client-only"]);
        git_ok(&reader, &["commit", "-m", "extra local base file"]);
        sandbox.run(&["rm", "dir"]);
        write(&sandbox.root, "dir/mine", b"portable addition");
        sandbox.run(&["add", "dir/mine"]);
        sandbox.run(&["commit", "-m", "mask dir"]);
        cli_code(&reader, &["pull", "--ff-only"], 0);
        assert!(!reader.join("dir/base").exists());
        assert!(!reader.join("dir/client-only").exists());
        assert_eq!(content(&reader, "dir/mine"), b"portable addition");
        assert!(git_ok(&reader, &["status", "--porcelain", "-uall"]).is_empty());
        sandbox.run(&["restore", "dir"]);
        sandbox.run(&["commit", "-m", "release dir"]);
        cli_code(&reader, &["pull", "--ff-only"], 0);
        assert_eq!(content(&reader, "dir/client-only"), b"local base");
        assert_eq!(content(&reader, "dir/base"), b"base");
        assert!(!reader.join("dir/mine").exists());
        assert_eq!(
            git_ok(&reader, &["ls-files", "-v", "--", "dir/client-only"]),
            b"H dir/client-only\n"
        );
        assert!(git_ok(&reader, &["status", "--porcelain", "-uall"]).is_empty());
    }

    #[test]
    fn ownership_lifecycle_keeps_base_clean_and_captures_only_overlay_content() {
        let sandbox = Sandbox::new(&[
            ("config", b"base\n"),
            ("obsolete", b"old\n"),
            ("untouched", b"safe\n"),
        ]);
        sandbox.init();
        write(&sandbox.root, "config", b"personal\n");
        write(&sandbox.root, "tools/dev", b"#!/bin/sh\n");
        sandbox.run(&["add", "config", "tools"]);
        sandbox.run(&["rm", "obsolete"]);
        sandbox.clean_base();
        sandbox.run(&["commit", "-m", "personal setup"]);
        let tree =
            String::from_utf8(sandbox.overlay(&["ls-tree", "-r", "--name-only", "HEAD"])).unwrap();
        assert!(tree.contains("config\n"));
        assert!(tree.contains("tools/dev\n"));
        assert!(!tree.contains("untouched\n"));
        assert!(!sandbox.root.join("obsolete").exists());
        assert_eq!(sandbox.overlay(&["show", "HEAD:config"]), b"personal\n");
        sandbox.run(&["restore", "--all"]);
        assert_eq!(content(&sandbox.root, "config"), b"base\n");
        assert_eq!(content(&sandbox.root, "obsolete"), b"old\n");
        assert!(!sandbox.root.join("tools/dev").exists());
        sandbox.clean_base();
    }

    #[test]
    fn restore_and_rm_reject_unstaged_overlay_edits_and_force_rescues_them() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"owned\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        write(&sandbox.root, "config", b"uncommitted unique bytes\n");
        sandbox.reject(&["restore", "config"]);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"uncommitted unique bytes\n"
        );
        sandbox.reject(&["rm", "config"]);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"uncommitted unique bytes\n"
        );
        sandbox.run(&["restore", "--force", "config"]);
        assert_eq!(content(&sandbox.root, "config"), b"base\n");
        assert!(
            backup_contents(&sandbox.root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"uncommitted unique bytes\n")
        );
        sandbox.clean_base();
    }

    #[test]
    fn unrelated_staged_and_unstaged_base_changes_survive_ownership_and_detach() {
        let sandbox = Sandbox::new(&[
            ("config", b"base\n"),
            ("staged", b"base\n"),
            ("local", b"base\n"),
        ]);
        sandbox.init();
        write(&sandbox.root, "staged", b"staged content\n");
        git_ok(&sandbox.root, &["add", "staged"]);
        write(&sandbox.root, "local", b"unstaged content\n");
        let index_before = git_ok(&sandbox.root, &["show", ":staged"]);
        write(&sandbox.root, "config", b"owned\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["detach"]);
        assert_eq!(content(&sandbox.root, "config"), b"base\n");
        assert_eq!(git_ok(&sandbox.root, &["show", ":staged"]), index_before);
        assert_eq!(content(&sandbox.root, "local"), b"unstaged content\n");
        sandbox.run(&["attach"]);
        sandbox.run(&["restore", "config"]);
        assert_eq!(git_ok(&sandbox.root, &["show", ":staged"]), index_before);
        assert_eq!(content(&sandbox.root, "local"), b"unstaged content\n");
    }

    #[test]
    fn staged_ownership_target_is_rejected_without_changing_index_or_file() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"staged bytes\n");
        git_ok(&sandbox.root, &["add", "config"]);
        let index_before = git_ok(&sandbox.root, &["ls-files", "--stage"]);
        sandbox.reject(&["add", "config"]);
        assert_eq!(
            git_ok(&sandbox.root, &["ls-files", "--stage"]),
            index_before
        );
        assert_eq!(content(&sandbox.root, "config"), b"staged bytes\n");
    }

    #[test]
    fn apply_dry_run_is_read_only_and_rescues_unexpected_bytes() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"owned\n");
        write(&sandbox.root, "added", b"added bytes\n");
        sandbox.run(&["add", "config", "added"]);
        sandbox.run(&["commit", "-m", "own"]);
        write(&sandbox.root, "config", b"unexpected local bytes\n");
        fs::remove_file(sandbox.root.join("added")).unwrap();
        cli_code(&sandbox.root, &["apply", "--dry-run"], 4);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"unexpected local bytes\n"
        );
        assert!(!sandbox.root.join("added").exists());
        assert!(!sandbox.root.join(".overgit/local/backups").exists());
        sandbox.run(&["apply"]);
        assert_eq!(content(&sandbox.root, "config"), b"owned\n");
        assert_eq!(content(&sandbox.root, "added"), b"added bytes\n");
        assert!(
            backup_contents(&sandbox.root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"unexpected local bytes\n")
        );
        cli_code(&sandbox.root, &["apply", "--dry-run"], 0);
        sandbox.clean_base();
    }

    #[test]
    fn detach_preserves_fresh_edits_and_missing_content_is_not_committed_as_deletion() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"owned\n");
        write(&sandbox.root, "added", b"recover me\n");
        sandbox.run(&["add", "config", "added"]);
        sandbox.run(&["commit", "-m", "own"]);
        fs::remove_file(sandbox.root.join("added")).unwrap();
        write(&sandbox.root, "config", b"fresh uncommitted bytes\n");
        sandbox.run(&["commit", "-m", "update config"]);
        assert_eq!(sandbox.overlay(&["show", "HEAD:added"]), b"recover me\n");
        write(&sandbox.root, "config", b"newer uncommitted bytes\n");
        sandbox.run(&["detach"]);
        assert_eq!(content(&sandbox.root, "config"), b"base\n");
        sandbox.run(&["detach"]);
        sandbox.run(&["attach"]);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"newer uncommitted bytes\n"
        );
        assert_eq!(content(&sandbox.root, "added"), b"recover me\n");
        sandbox.clean_base();
    }

    #[test]
    fn literal_paths_exact_ignore_patterns_symlinks_and_executable_modes_round_trip() {
        let sandbox = Sandbox::new(&[("tracked\nname", b"base\n")]);
        sandbox.init();
        for path in ["dir/.hidden", "dir/a*b[1]", "dir/space tab\t", "-option"] {
            write(&sandbox.root, path, path.as_bytes());
        }
        write(&sandbox.root, "tracked\nname", b"override\n");
        write(&sandbox.root, "run.sh", b"#!/bin/sh\nexit 0\n");
        fs::set_permissions(
            sandbox.root.join("run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("not-existing-target", sandbox.root.join("link")).unwrap();
        sandbox.run(&[
            "add",
            "dir",
            "run.sh",
            "link",
            "tracked\nname",
            "--",
            "-option",
        ]);
        write(&sandbox.root, "dir/axxb1", b"must remain visible\n");
        let status = git_ok(
            &sandbox.root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        assert!(String::from_utf8_lossy(&status).contains("dir/axxb1"));
        sandbox.run(&["commit", "-m", "special files"]);
        sandbox.run(&["detach"]);
        assert!(!sandbox.root.join("run.sh").exists());
        sandbox.run(&["attach"]);
        assert_eq!(
            fs::read_link(sandbox.root.join("link")).unwrap(),
            Path::new("not-existing-target")
        );
        assert_ne!(
            fs::metadata(sandbox.root.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(content(&sandbox.root, "tracked\nname"), b"override\n");
    }

    #[test]
    fn path_escape_metadata_and_unrepresentable_added_names_are_rejected() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        let outside = sandbox.temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        write(&outside, "secret", b"untouched\n");
        symlink(&outside, sandbox.root.join("escape")).unwrap();
        sandbox.reject(&["add", "escape/secret"]);
        sandbox.reject(&["add", "../outside/secret"]);
        sandbox.reject(&["add", ".git/config"]);
        sandbox.reject(&["add", ".overgit/manifest.json"]);
        write(&sandbox.root, "bad\nname", b"bytes\n");
        sandbox.reject(&["add", "bad\nname"]);
        assert_eq!(content(&outside, "secret"), b"untouched\n");
    }

    #[test]
    fn invalid_batch_is_validated_before_any_ownership_change() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"personal\n");
        let manifest = content(&sandbox.root, ".overgit/manifest.json");
        sandbox.reject(&["add", "config", ".git/config"]);
        assert_eq!(content(&sandbox.root, ".overgit/manifest.json"), manifest);
        assert_eq!(content(&sandbox.root, "config"), b"personal\n");
        assert!(!git_ok(&sandbox.root, &["ls-files", "-v", "config"]).starts_with(b"S "));
    }

    #[test]
    fn sparse_and_busy_base_refuse_mutation_while_status_remains_available() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"personal\n");
        git_ok(&sandbox.root, &["config", "core.sparseCheckout", "true"]);
        sandbox.reject(&["add", "config"]);
        git_ok(&sandbox.root, &["config", "core.sparseCheckout", "false"]);
        write(
            &sandbox.root,
            ".git/MERGE_HEAD",
            b"0000000000000000000000000000000000000000\n",
        );
        sandbox.reject(&["add", "config"]);
        sandbox.run(&["status"]);
        assert_eq!(content(&sandbox.root, "config"), b"personal\n");
    }

    #[test]
    fn clean_three_way_merge_keeps_both_sides_and_advances_fork() {
        let sandbox = Sandbox::new(&[("config", b"one\ntwo\nthree\nfour\nfive\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"mine\ntwo\nthree\nfour\nfive\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.upstream(&[("config", Some(b"one\ntwo\nthree\nfour\nupstream\n"))]);
        cli_code(&sandbox.root, &["sync", "--dry-run"], 4);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"mine\ntwo\nthree\nfour\nfive\n"
        );
        sandbox.run(&["sync"]);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"mine\ntwo\nthree\nfour\nupstream\n"
        );
        cli_code(&sandbox.root, &["sync", "--dry-run"], 0);
        sandbox.clean_base();
    }

    #[test]
    fn sync_conflicts_do_not_block_other_paths_and_resolve_rejects_markers() {
        let sandbox = Sandbox::new(&[
            ("conflict", b"base\n"),
            ("clean", b"one\ntwo\nthree\nfour\nfive\n"),
        ]);
        sandbox.init();
        write(&sandbox.root, "conflict", b"mine\n");
        write(&sandbox.root, "clean", b"mine\ntwo\nthree\nfour\nfive\n");
        sandbox.run(&["add", "conflict", "clean"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.upstream(&[
            ("conflict", Some(b"upstream\n")),
            ("clean", Some(b"one\ntwo\nthree\nfour\nupstream\n")),
        ]);
        cli_code(&sandbox.root, &["sync", "--style", "diff3"], 3);
        assert!(String::from_utf8_lossy(&content(&sandbox.root, "conflict")).contains("<<<<<<<"));
        assert_eq!(
            content(&sandbox.root, "clean"),
            b"mine\ntwo\nthree\nfour\nupstream\n"
        );
        sandbox.reject(&["resolve", "conflict"]);
        sandbox.reject(&["commit", "-m", "must refuse"]);
        sandbox.run(&["status"]);
        write(&sandbox.root, "conflict", b"hand resolved\n");
        sandbox.run(&["resolve", "conflict"]);
        sandbox.run(&["sync", "--continue"]);
        sandbox.run(&["commit", "-m", "merged"]);
        sandbox.clean_base();
    }

    #[test]
    fn sync_abort_restores_pre_sync_uncommitted_work_and_index_exactly() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"owned\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.upstream(&[("config", Some(b"upstream\n"))]);
        write(&sandbox.root, "config", b"uncommitted before sync\n");
        let manifest_before = content(&sandbox.root, ".overgit/manifest.json");
        let index_before = sandbox.overlay(&["ls-files", "--stage", "-z"]);
        cli_code(&sandbox.root, &["sync"], 3);
        write(&sandbox.root, "config", b"partial conflict edit\n");
        sandbox.run(&["sync", "--abort"]);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"uncommitted before sync\n"
        );
        assert_eq!(
            content(&sandbox.root, ".overgit/manifest.json"),
            manifest_before
        );
        assert_eq!(
            sandbox.overlay(&["ls-files", "--stage", "-z"]),
            index_before
        );
        sandbox.clean_base();
    }

    #[test]
    fn upstream_deletion_collision_and_binary_conflict_require_explicit_decisions() {
        let sandbox = Sandbox::new(&[("deleted", b"base\n"), ("binary", b"base\0bytes")]);
        sandbox.init();
        write(&sandbox.root, "deleted", b"mine\n");
        write(&sandbox.root, "binary", b"mine\0bytes");
        write(&sandbox.root, "collision", b"mine added\n");
        sandbox.run(&["add", "deleted", "binary", "collision"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.upstream(&[
            ("deleted", None),
            ("binary", Some(b"upstream\0bytes")),
            ("collision", Some(b"upstream added\n")),
        ]);
        cli_code(&sandbox.root, &["sync"], 3);
        assert_eq!(content(&sandbox.root, "binary"), b"mine\0bytes");
        cli_code(&sandbox.root, &["resolve", "--keep", "deleted"], 3);
        cli_code(&sandbox.root, &["resolve", "--take-upstream", "binary"], 3);
        sandbox.run(&["resolve", "--drop", "collision"]);
        sandbox.run(&["sync", "--continue"]);
        assert_eq!(content(&sandbox.root, "deleted"), b"mine\n");
        assert_eq!(content(&sandbox.root, "binary"), b"upstream\0bytes");
        assert_eq!(content(&sandbox.root, "collision"), b"upstream added\n");
        let list = sandbox.run(&["list", "--kind", "add"]).stdout;
        assert!(String::from_utf8_lossy(&list).contains("deleted"));
        sandbox.clean_base();
    }

    #[test]
    fn whiteouts_survive_upstream_changes_and_disappear_after_upstream_deletion() {
        let sandbox = Sandbox::new(&[("hidden", b"base\n")]);
        sandbox.init();
        sandbox.run(&["rm", "hidden"]);
        sandbox.run(&["commit", "-m", "hide"]);
        sandbox.upstream(&[("hidden", Some(b"upstream\n"))]);
        sandbox.run(&["sync"]);
        assert!(!sandbox.root.join("hidden").exists());
        sandbox.upstream(&[("hidden", None)]);
        sandbox.run(&["sync"]);
        assert!(!sandbox.root.join("hidden").exists());
        assert!(!String::from_utf8_lossy(&sandbox.run(&["list"]).stdout).contains("hidden"));
        sandbox.clean_base();
    }

    #[test]
    fn local_clone_round_trip_push_fetch_pull_and_failed_git_exit_status() {
        let sandbox = Sandbox::new(&[("config", b"base\n"), ("hidden", b"base\n")]);
        let overlay_remote = sandbox.temporary.path().join("overlay.git");
        git_ok(
            sandbox.temporary.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                overlay_remote.to_str().unwrap(),
            ],
        );
        cli_code(
            &sandbox.root,
            &["init", "--remote", overlay_remote.to_str().unwrap()],
            0,
        );
        write(&sandbox.root, "config", b"mine\n");
        write(&sandbox.root, "added", b"mine added\n");
        sandbox.run(&["add", "config", "added"]);
        sandbox.run(&["rm", "hidden"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.run(&["push", "-u", "origin", "main"]);
        let cloned = sandbox.temporary.path().join("cloned");
        let clone_arguments = [
            "clone",
            overlay_remote.to_str().unwrap(),
            cloned.to_str().unwrap(),
            "--base",
            sandbox.root.to_str().unwrap(),
        ];
        cli_code(sandbox.temporary.path(), &clone_arguments, 0);
        assert_eq!(content(&cloned, "config"), b"mine\n");
        assert_eq!(content(&cloned, "added"), b"mine added\n");
        assert!(!cloned.join("hidden").exists());
        cli_code(sandbox.temporary.path(), &clone_arguments, 0);
        cli_code(&cloned, &["fetch"], 0);
        write(&sandbox.root, "config", b"published update\n");
        sandbox.run(&["commit", "-m", "update"]);
        sandbox.run(&["push"]);
        cli_code(&cloned, &["pull", "--ff-only"], 0);
        assert_eq!(content(&cloned, "config"), b"published update\n");
        write(&cloned, "config", b"uncommitted after pull\n");
        let absent_remote = sandbox.temporary.path().join("absent-remote.git");
        cli_code(
            &cloned,
            &[
                "git",
                "remote",
                "set-url",
                "origin",
                absent_remote.to_str().unwrap(),
            ],
            0,
        );
        let failed = cli(&cloned, &["pull", "--ff-only"]);
        assert!(!failed.status.success());
        assert_eq!(content(&cloned, "config"), b"uncommitted after pull\n");
        sandbox.reject(&["git", "definitely-not-a-git-command"]);
    }

    #[test]
    fn clone_into_existing_base_rescues_unexpected_content() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"mine\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        let other = sandbox.temporary.path().join("other");
        git_ok(
            sandbox.temporary.path(),
            &[
                "clone",
                sandbox.root.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        write(&other, "config", b"existing uncommitted bytes\n");
        cli_code(
            &other,
            &[
                "clone",
                sandbox.root.join(".overgit/.git").to_str().unwrap(),
            ],
            0,
        );
        assert_eq!(content(&other, "config"), b"mine\n");
        assert!(
            backup_contents(&other.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"existing uncommitted bytes\n")
        );
    }

    #[test]
    fn linked_worktrees_share_history_and_excludes_but_refuse_dirty_removal() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"mine\n");
        write(&sandbox.root, "primary-added", b"primary\n");
        sandbox.run(&["add", "config", "primary-added"]);
        sandbox.run(&["commit", "-m", "own"]);
        let linked = sandbox.temporary.path().join("linked");
        sandbox.run(&["worktree", "add", linked.to_str().unwrap(), "-b", "linked"]);
        assert_eq!(content(&linked, "config"), b"mine\n");
        assert_eq!(content(&linked, "primary-added"), b"primary\n");
        write(&linked, "linked-added", b"linked\n");
        cli_code(&linked, &["add", "linked-added"], 0);
        git_ok(&sandbox.root, &["check-ignore", "linked-added"]);
        sandbox.reject(&["worktree", "remove", linked.to_str().unwrap()]);
        assert!(linked.exists());
        cli_code(&linked, &["commit", "-m", "linked additions"], 0);
        let list = sandbox.run(&["worktree", "list"]).stdout;
        assert!(String::from_utf8_lossy(&list).contains("linked"));
        sandbox.run(&["worktree", "remove", linked.to_str().unwrap()]);
        assert!(!linked.exists());
        assert!(
            !git(&sandbox.root, &["check-ignore", "linked-added"])
                .status
                .success()
        );
        sandbox.clean_base();
    }

    #[test]
    fn hooks_preserve_user_shell_code_and_installation_is_idempotent() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        write(
            &sandbox.root,
            ".git/hooks/post-checkout",
            b"#!/bin/sh\necho user-hook >/dev/null\n",
        );
        sandbox.run(&["hooks", "install"]);
        let first = content(&sandbox.root, ".git/hooks/post-checkout");
        assert!(String::from_utf8_lossy(&first).contains("echo user-hook"));
        sandbox.run(&["hooks", "install"]);
        assert_eq!(content(&sandbox.root, ".git/hooks/post-checkout"), first);
        sandbox.run(&["hooks", "uninstall"]);
        let after = content(&sandbox.root, ".git/hooks/post-checkout");
        assert!(String::from_utf8_lossy(&after).contains("echo user-hook"));
        assert!(!String::from_utf8_lossy(&after).contains("overgit apply"));
    }

    #[test]
    fn early_exit_hook_sees_applied_overlay_and_survives_exact_roundtrip() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        write(&sandbox.root, "added", b"saved\n");
        sandbox.run(&["add", "added"]);
        let original = b"#!/bin/sh\ntest -e added || exit 9\nprintf '%s\\n' \"$3\" > hook-args\nexit 0 # no final newline";
        write(&sandbox.root, ".git/hooks/post-checkout", original);
        sandbox.run(&["hooks", "install"]);
        let installed = content(&sandbox.root, ".git/hooks/post-checkout");
        sandbox.run(&["hooks", "install"]);
        assert_eq!(
            installed,
            content(&sandbox.root, ".git/hooks/post-checkout")
        );
        fs::remove_file(sandbox.root.join("added")).unwrap();
        git_ok(&sandbox.root, &["checkout", "-b", "next"]);
        assert_eq!(content(&sandbox.root, "added"), b"saved\n");
        assert_eq!(content(&sandbox.root, "hook-args"), b"1\n");
        sandbox.run(&["hooks", "uninstall"]);
        assert_eq!(content(&sandbox.root, ".git/hooks/post-checkout"), original);
        sandbox.run(&["hooks", "uninstall"]);
        assert_eq!(content(&sandbox.root, ".git/hooks/post-checkout"), original);
    }

    #[test]
    fn hook_prefix_keeps_startup_and_user_traps_in_one_shell() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        write(&sandbox.root, "added", b"saved\n");
        sandbox.run(&["add", "added"]);
        let hook = ".git/hooks/post-checkout";
        write(
            &sandbox.root,
            "startup",
            b"echo startup >> startup-count\ntrap 'echo old >> trap-log' EXIT\n",
        );
        write(&sandbox.root, hook, b"#!/bin/bash -eu\ntrap 'echo user >> trap-log' EXIT\ntest -e added\necho user-edit > added\nexit 6\n");
        sandbox.run(&["hooks", "install"]);
        fs::remove_file(sandbox.root.join("added")).unwrap();
        let output = isolated(Command::new(format!("./{hook}")).current_dir(&sandbox.root))
            .env("BASH_ENV", sandbox.root.join("startup"))
            .args(["old", "new", "1"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(6),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(content(&sandbox.root, "startup-count"), b"startup\n");
        assert_eq!(content(&sandbox.root, "trap-log"), b"user\n");
        assert_eq!(content(&sandbox.root, "added"), b"user-edit\n");
    }

    #[test]
    fn hook_apply_failure_does_not_replace_user_exit_status() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        let hook = ".git/hooks/post-checkout";
        write(
            &sandbox.root,
            hook,
            b"#!/bin/sh -eu\necho ran > user-ran\nexit 5\n",
        );
        sandbox.run(&["hooks", "install"]);
        write(
            &sandbox.root,
            ".overgit/manifest.json",
            b"invalid manifest\n",
        );
        let output = isolated(Command::new(format!("./{hook}")).current_dir(&sandbox.root))
            .args(["old", "new", "1"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(5));
        assert_eq!(content(&sandbox.root, "user-ran"), b"ran\n");
        assert!(
            !output.stderr.is_empty(),
            "apply failure should remain visible"
        );
    }

    #[test]
    fn hooks_roundtrip_shebangless_empty_and_legacy_content() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        let hook = ".git/hooks/post-checkout";
        for original in [
            "",
            "#!/bin/sh",
            "exit 0 # final comment",
            "#!/bin/sh\nexit 0\n",
        ] {
            write(&sandbox.root, hook, original.as_bytes());
            sandbox.run(&["hooks", "install"]);
            let installed = content(&sandbox.root, hook);
            sandbox.run(&["hooks", "install"]);
            assert_eq!(content(&sandbox.root, hook), installed);
            sandbox.run(&["hooks", "uninstall"]);
            assert_eq!(content(&sandbox.root, hook), original.as_bytes());
        }
        write(&sandbox.root, hook, b"#!/bin/sh");
        sandbox.run(&["hooks", "install"]);
        let mut edited = content(&sandbox.root, hook);
        edited.extend_from_slice(b"echo user-edit\n");
        write(&sandbox.root, hook, &edited);
        sandbox.run(&["hooks", "uninstall"]);
        assert_eq!(content(&sandbox.root, hook), b"#!/bin/sh\necho user-edit\n");
        let original = "#!/bin/sh\nexit 0\n";
        write(&sandbox.root, hook, format!("{original}# BEGIN overgit managed hook\necho unreachable\n# END overgit managed hook\n").as_bytes());
        sandbox.run(&["hooks", "install"]);
        assert!(
            !String::from_utf8(content(&sandbox.root, hook))
                .unwrap()
                .contains("unreachable")
        );
        sandbox.run(&["hooks", "uninstall"]);
        assert_eq!(content(&sandbox.root, hook), original.as_bytes());
    }

    #[test]
    fn hook_chaining_preserves_shell_options_status_arguments_stdin_and_traps() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        sandbox.run(&["git", "config", "user.name", "Hook Test"]);
        write(&sandbox.root, "added", b"saved\n");
        sandbox.run(&["add", "added"]);
        git_ok(
            &sandbox.root,
            &["config", "core.hooksPath", "hooks with spaces"],
        );
        let hook = "hooks with spaces/post-rewrite";
        for (original, expected) in [
            (
                "#!/bin/sh -eu\nprintf '%s\\n' \"$0\" \"$@\" > hook-args\ncat > hook-input\ntrap 'echo trapped > hook-trap' EXIT\nfalse\necho bad > hook-after\n",
                1,
            ),
            (
                "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > hook-args\ncat > hook-input\nexec sh -c 'exit 7'\n",
                7,
            ),
            (
                "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > hook-args\ncat > hook-input\ncat <<'END' > hook-heredoc\nbody ( text\nEND\nexit 3",
                3,
            ),
        ] {
            write(&sandbox.root, hook, original.as_bytes());
            sandbox.run(&["hooks", "install"]);
            fs::remove_file(sandbox.root.join("added")).unwrap();
            let mut child = isolated(Command::new(format!("./{hook}")).current_dir(&sandbox.root))
                .args(["amend", "argument with spaces"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"rewrite input\nsecond line\n")
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(content(&sandbox.root, "added"), b"saved\n");
            assert_eq!(
                content(&sandbox.root, "hook-input"),
                b"rewrite input\nsecond line\n"
            );
            assert_eq!(
                content(&sandbox.root, "hook-args"),
                format!("./{hook}\namend\nargument with spaces\n").as_bytes()
            );
            assert!(!sandbox.root.join("hook-after").exists());
            sandbox.run(&["hooks", "uninstall"]);
            assert_eq!(content(&sandbox.root, hook), original.as_bytes());
        }
        assert_eq!(content(&sandbox.root, "hook-trap"), b"trapped\n");
        assert_eq!(content(&sandbox.root, "hook-heredoc"), b"body ( text\n");
    }

    #[test]
    fn doctor_repairs_missing_content_and_owned_flags_without_losing_ordinary_edits() {
        let sandbox = Sandbox::new(&[("config", b"base\n"), ("unowned", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"mine\n");
        write(&sandbox.root, "added", b"recover\n");
        sandbox.run(&["add", "config", "added"]);
        sandbox.run(&["commit", "-m", "own"]);
        write(&sandbox.root, "config", b"ordinary uncommitted edit\n");
        git_ok(
            &sandbox.root,
            &["update-index", "--no-skip-worktree", "config"],
        );
        git_ok(
            &sandbox.root,
            &["update-index", "--skip-worktree", "unowned"],
        );
        fs::remove_file(sandbox.root.join("added")).unwrap();
        cli_code(&sandbox.root, &["doctor"], 4);
        cli_code(&sandbox.root, &["doctor", "--fix"], 4);
        assert_eq!(
            content(&sandbox.root, "config"),
            b"ordinary uncommitted edit\n"
        );
        assert_eq!(content(&sandbox.root, "added"), b"recover\n");
        assert!(git_ok(&sandbox.root, &["ls-files", "-v", "config"]).starts_with(b"S "));
        assert!(git_ok(&sandbox.root, &["ls-files", "-v", "unowned"]).starts_with(b"S "));
    }

    #[test]
    fn malformed_manifest_does_not_become_an_empty_overlay() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"mine\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        write(&sandbox.root, ".overgit/manifest.json", b"{ invalid json");
        sandbox.reject(&["apply"]);
        assert_eq!(content(&sandbox.root, "config"), b"mine\n");
        sandbox.run(&["doctor", "--fix"]);
        assert_eq!(content(&sandbox.root, "config"), b"mine\n");
        assert!(String::from_utf8_lossy(&sandbox.run(&["list"]).stdout).contains("config"));
    }

    #[test]
    fn ambient_git_context_cannot_redirect_overgit_to_another_repository() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        let other = Sandbox::new(&[("secret", b"outside\n")]);
        let before = git_ok(&other.root, &["ls-files", "--stage"]);
        let output =
            isolated(Command::new(env!("CARGO_BIN_EXE_overgit")).current_dir(&sandbox.root))
                .env("GIT_DIR", other.root.join(".git"))
                .env("GIT_WORK_TREE", &other.root)
                .env("GIT_INDEX_FILE", other.root.join(".git/index"))
                .args(["init"])
                .output()
                .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(sandbox.root.join(".overgit/.git").exists());
        assert!(!other.root.join(".overgit").exists());
        assert_eq!(git_ok(&other.root, &["ls-files", "--stage"]), before);
    }

    #[test]
    fn apply_force_rescues_obstructing_directory_recursively() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        write(&sandbox.root, "owned", b"overlay content\n");
        sandbox.run(&["add", "owned"]);
        sandbox.run(&["commit", "-m", "own"]);
        fs::remove_file(sandbox.root.join("owned")).unwrap();
        write(&sandbox.root, "owned/nested/valuable", b"directory data\n");
        sandbox.reject(&["apply"]);
        assert_eq!(
            content(&sandbox.root, "owned/nested/valuable"),
            b"directory data\n"
        );
        sandbox.run(&["apply", "--force"]);
        assert_eq!(content(&sandbox.root, "owned"), b"overlay content\n");
        assert!(
            backup_contents(&sandbox.root.join(".overgit/local/backups"))
                .iter()
                .any(|bytes| bytes == b"directory data\n")
        );
    }

    #[test]
    fn submodule_contents_are_rejected_without_touching_the_submodule() {
        let sandbox = Sandbox::new(&[]);
        let source = sandbox.temporary.path().join("submodule-source");
        fs::create_dir(&source).unwrap();
        git_ok(&source, &["init", "--initial-branch=main"]);
        write(&source, "tracked", b"submodule bytes\n");
        git_ok(&source, &["add", "--all"]);
        git_ok(&source, &["commit", "-m", "submodule"]);
        git_ok(
            &sandbox.root,
            &["submodule", "add", source.to_str().unwrap(), "module"],
        );
        git_ok(&sandbox.root, &["commit", "-m", "add submodule"]);
        sandbox.init();
        sandbox.reject(&["add", "module/tracked"]);
        sandbox.run(&["add", "."]);
        assert!(
            !String::from_utf8_lossy(&sandbox.run(&["list", "--porcelain"]).stdout)
                .contains("module/")
        );
        assert_eq!(
            content(&sandbox.root, "module/tracked"),
            b"submodule bytes\n"
        );
        sandbox.clean_base();
    }

    #[test]
    fn foreign_language_hooks_are_preserved_and_custom_hook_directory_is_supported() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        git_ok(&sandbox.root, &["config", "core.hooksPath", "custom-hooks"]);
        let original = b"#!/usr/bin/env python3\nprint('preserve me')\n";
        write(&sandbox.root, "custom-hooks/post-checkout", original);
        sandbox.run(&["hooks", "install"]);
        assert_eq!(
            content(&sandbox.root, "custom-hooks/post-checkout"),
            original
        );
        assert!(sandbox.root.join("custom-hooks/post-merge").exists());
        sandbox.run(&["hooks", "uninstall"]);
        assert_eq!(
            content(&sandbox.root, "custom-hooks/post-checkout"),
            original
        );
    }

    #[test]
    fn restore_keep_file_unowns_an_add_and_preserves_its_bytes() {
        let sandbox = Sandbox::new(&[]);
        sandbox.init();
        write(&sandbox.root, "keep", b"keep these bytes\n");
        sandbox.run(&["add", "keep"]);
        sandbox.run(&["restore", "--keep-file", "keep"]);
        assert_eq!(content(&sandbox.root, "keep"), b"keep these bytes\n");
        assert!(
            !git(&sandbox.root, &["check-ignore", "keep"])
                .status
                .success()
        );
        assert!(
            String::from_utf8_lossy(&git_ok(
                &sandbox.root,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ))
            .contains("keep")
        );
    }

    #[test]
    fn restore_keep_file_preserves_an_add_that_upstream_now_tracks() {
        let sandbox = Sandbox::new(&[]);
        let root = &sandbox.root;
        sandbox.init();
        write(root, "config", b"personal\n");
        sandbox.run(&["add", "config"]);
        sandbox.upstream(&[("config", Some(b"new base\n"))]);
        assert_eq!(content(root, "config"), b"personal\n");

        sandbox.run(&["restore", "--keep-file", "config"]);
        assert_eq!(content(root, "config"), b"personal\n");
        assert!(sandbox.run(&["list", "--porcelain"]).stdout.is_empty());
        assert_eq!(
            git_ok(root, &["status", "--porcelain=v1", "--", "config"]),
            b" M config\n"
        );
    }

    #[test]
    fn sync_only_leaves_other_paths_unchanged_and_keep_resolves_conflict() {
        let sandbox = Sandbox::new(&[("first", b"base\n"), ("second", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "first", b"first mine\n");
        write(&sandbox.root, "second", b"second mine\n");
        sandbox.run(&["add", "first", "second"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.upstream(&[
            ("first", Some(b"first upstream\n")),
            ("second", Some(b"second upstream\n")),
        ]);
        cli_code(&sandbox.root, &["sync", "--only", "first"], 3);
        assert_eq!(content(&sandbox.root, "second"), b"second mine\n");
        sandbox.run(&["resolve", "--keep", "first"]);
        sandbox.run(&["sync", "--continue"]);
        assert_eq!(content(&sandbox.root, "first"), b"first mine\n");
        cli_code(&sandbox.root, &["sync", "--only", "first", "--dry-run"], 0);
        cli_code(&sandbox.root, &["sync", "--dry-run"], 4);
    }

    #[test]
    fn status_omits_unchanged_owned_paths_unless_requested() {
        let sandbox = Sandbox::new(&[("hidden", b"base"), ("edited", b"base")]);
        sandbox.init();
        sandbox.run(&["rm", "hidden"]);
        sandbox.run(&["add", "edited"]);
        sandbox.run(&["commit", "-m", "overlay"]);
        write(&sandbox.root, "edited", b"dirty");
        for args in [vec!["status"], vec!["status", "--short"]] {
            let output = String::from_utf8(sandbox.run(&args).stdout).unwrap();
            assert!(!output.contains("\"hidden\""));
            assert!(output.contains("\"edited\""));
        }
        for flag in ["--all", "--porcelain"] {
            let output = String::from_utf8(sandbox.run(&["status", flag]).stdout).unwrap();
            assert!(output.contains("\"hidden\""));
        }
    }

    #[test]
    fn inspection_commands_work_from_subdirectories_and_global_directory_flag() {
        let sandbox = Sandbox::new(&[("config", b"base\n"), ("nested/base", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "nested/owned", b"mine\n");
        cli_code(&sandbox.root.join("nested"), &["add", "owned"], 0);
        let list = sandbox.run(&["list", "--porcelain"]).stdout;
        assert!(
            list.windows(b"nested/owned".len())
                .any(|window| window == b"nested/owned")
        );
        let which = cli_code(&sandbox.root.join("nested"), &["which", "owned"], 0);
        assert!(String::from_utf8_lossy(&which.stdout).contains("overlay"));
        sandbox.run(&["status", "--short"]);
        sandbox.run(&["status", "--porcelain"]);
        cli_code(
            sandbox.temporary.path(),
            &["-C", sandbox.root.to_str().unwrap(), "list"],
            0,
        );
        sandbox.run(&["commit", "-m", "nested"]);
        let log = sandbox.run(&["log", "--oneline"]).stdout;
        assert!(String::from_utf8_lossy(&log).contains("nested"));
        write(&sandbox.root, "nested/owned", b"edited\n");
        assert!(String::from_utf8_lossy(&sandbox.run(&["diff"]).stdout).contains("edited"));
    }
    #[test]
    fn installed_hooks_reapply_missing_overlay_files_after_base_checkout() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "added", b"restore through hook\n");
        sandbox.run(&["add", "added"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.run(&["hooks", "install"]);
        fs::remove_file(sandbox.root.join("added")).unwrap();
        git_ok(&sandbox.root, &["checkout", "--detach", "HEAD"]);
        assert_eq!(content(&sandbox.root, "added"), b"restore through hook\n");
        sandbox.clean_base();
    }

    #[test]
    fn status_reports_fetched_base_changes_that_would_block_a_pull() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        let remote = sandbox.temporary.path().join("base-remote.git");
        git_ok(
            sandbox.temporary.path(),
            &[
                "clone",
                "--bare",
                sandbox.root.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git_ok(
            &sandbox.root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_ok(&sandbox.root, &["fetch", "origin"]);
        git_ok(
            &sandbox.root,
            &["branch", "--set-upstream-to=origin/main", "main"],
        );
        sandbox.init();
        write(&sandbox.root, "config", b"mine\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        let writer = sandbox.temporary.path().join("writer");
        git_ok(
            sandbox.temporary.path(),
            &["clone", remote.to_str().unwrap(), writer.to_str().unwrap()],
        );
        write(&writer, "config", b"upstream\n");
        git_ok(&writer, &["commit", "-am", "upstream"]);
        git_ok(&writer, &["push"]);
        git_ok(&sandbox.root, &["fetch", "origin"]);
        let report: serde_json::Value =
            serde_json::from_slice(&sandbox.run(&["status", "--porcelain"]).stdout).unwrap();
        assert_eq!(report["base"]["behind"], 1);
        assert_eq!(report["pull_blocked"], serde_json::json!(["config"]));
        assert_eq!(content(&sandbox.root, "config"), b"mine\n");
    }
    #[test]
    fn conflicting_overlay_pull_leaves_original_head_index_and_worktree_intact() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
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
        cli_code(
            &sandbox.root,
            &["init", "--remote", remote.to_str().unwrap()],
            0,
        );
        write(&sandbox.root, "config", b"initial overlay\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "initial"]);
        sandbox.run(&["push", "-u", "origin", "main"]);
        let other = sandbox.temporary.path().join("other");
        git_ok(
            sandbox.temporary.path(),
            &[
                "clone",
                sandbox.root.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        cli_code(&other, &["clone", remote.to_str().unwrap()], 0);
        write(&other, "config", b"remote conflicting commit\n");
        cli_code(&other, &["commit", "-m", "remote change"], 0);
        cli_code(&other, &["push"], 0);
        write(&sandbox.root, "config", b"local conflicting commit\n");
        sandbox.run(&["commit", "-m", "local change"]);
        let head_before = sandbox.overlay(&["rev-parse", "HEAD"]);
        let index_before = sandbox.overlay(&["ls-files", "--stage", "-z"]);
        let manifest_before = content(&sandbox.root, ".overgit/manifest.json");
        let base_index_before = git_ok(&sandbox.root, &["ls-files", "--stage", "-z"]);
        sandbox.reject(&["pull", "--no-rebase", "origin", "main"]);
        assert_eq!(sandbox.overlay(&["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            sandbox.overlay(&["ls-files", "--stage", "-z"]),
            index_before
        );
        assert_eq!(
            content(&sandbox.root, "config"),
            b"local conflicting commit\n"
        );
        assert_eq!(
            content(&sandbox.root, ".overgit/manifest.json"),
            manifest_before
        );
        assert_eq!(
            git_ok(&sandbox.root, &["ls-files", "--stage", "-z"]),
            base_index_before
        );
        assert!(!sandbox.root.join(".overgit/.git/MERGE_HEAD").exists());
        sandbox.clean_base();
    }

    #[test]
    fn pending_upstream_deletion_keeps_overlay_bytes_out_of_base_add_all() {
        let sandbox = Sandbox::new(&[("config", b"base\n")]);
        sandbox.init();
        write(&sandbox.root, "config", b"mine\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.upstream(&[("config", None)]);
        cli_code(&sandbox.root, &["sync"], 3);
        assert_eq!(content(&sandbox.root, "config"), b"mine\n");
        git_ok(&sandbox.root, &["add", "--all"]);
        assert!(git_ok(&sandbox.root, &["diff", "--cached", "--name-only"]).is_empty());
        sandbox.clean_base();
        sandbox.run(&["resolve", "--keep", "config"]);
        sandbox.clean_base();
    }

    #[test]
    fn detach_uses_base_checkout_filters_for_autocrlf_content() {
        let sandbox = Sandbox::new(&[("config", b"base\nsecond\n")]);
        git_ok(&sandbox.root, &["config", "core.autocrlf", "true"]);
        // Materialize exactly what the base's checkout considers pristine.
        fs::remove_file(sandbox.root.join("config")).unwrap();
        git_ok(&sandbox.root, &["checkout", "--", "config"]);
        assert_eq!(content(&sandbox.root, "config"), b"base\r\nsecond\r\n");
        sandbox.init();
        write(&sandbox.root, "config", b"mine\r\nsecond\r\n");
        sandbox.run(&["add", "config"]);
        sandbox.run(&["commit", "-m", "own"]);
        sandbox.run(&["detach"]);
        assert_eq!(content(&sandbox.root, "config"), b"base\r\nsecond\r\n");
        assert!(git_ok(&sandbox.root, &["status", "--porcelain=v1"]).is_empty());
        sandbox.run(&["attach"]);
        assert_eq!(content(&sandbox.root, "config"), b"mine\r\nsecond\r\n");
    }
}
