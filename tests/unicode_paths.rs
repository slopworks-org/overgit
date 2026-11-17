#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        process::{Command, Output},
    };

    fn run(root: &Path, program: &str, args: &[&str], code: i32) -> Output {
        let output = Command::new(program)
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Unicode Test")
            .env("GIT_AUTHOR_EMAIL", "unicode@example.invalid")
            .env("GIT_COMMITTER_NAME", "Unicode Test")
            .env("GIT_COMMITTER_EMAIL", "unicode@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn cli(root: &Path, args: &[&str], code: i32) -> Output {
        run(root, env!("CARGO_BIN_EXE_overgit"), args, code)
    }
    fn init(root: &Path, precompose: &str) {
        run(root, "git", &["init", "--initial-branch=main"], 0);
        run(
            root,
            "git",
            &["config", "core.precomposeunicode", precompose],
            0,
        );
        run(root, "git", &["commit", "--allow-empty", "-m", "base"], 0);
        cli(root, &["init"], 0);
        cli(
            root,
            &["git", "config", "core.precomposeunicode", precompose],
            0,
        );
    }
    fn state(root: &Path) -> Vec<Vec<u8>> {
        [
            ".overgit/manifest.json",
            ".overgit/.git/index",
            ".git/index",
            ".git/info/exclude",
        ]
        .iter()
        .map(|path| fs::read(root.join(path)).unwrap())
        .collect()
    }
    #[test]
    fn canonical_alias_is_refused_before_losing_the_first_staged_version() {
        for precompose in ["true", "false"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            init(root, precompose);
            fs::write(root.join("é"), b"first staged only\n").unwrap();
            cli(root, &["add", "é"], 0);
            let before = state(root);
            fs::write(root.join("e\u{301}"), b"second working bytes\n").unwrap();
            cli(root, &["add", "e\u{301}"], 1);
            assert_eq!(state(root), before);
            cli(root, &["git", "gc", "--prune=now"], 0);
            assert_eq!(
                cli(root, &["git", "show", ":é"], 0).stdout,
                b"first staged only\n"
            );
            assert_eq!(
                fs::read(root.join("e\u{301}")).unwrap(),
                b"second working bytes\n"
            );
        }
    }
    #[test]
    fn single_decomposed_path_keeps_exact_spelling_through_owned_lifecycle() {
        for precompose in ["true", "false"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            init(root, precompose);
            let name = "e\u{301}";
            fs::write(root.join(name), b"private\n").unwrap();
            cli(root, &["add", name], 0);
            cli(root, &["commit", "-m", "decomposed"], 0);
            let indexed = cli(
                root,
                &[
                    "git",
                    "-c",
                    "core.precomposeunicode=false",
                    "ls-files",
                    "-z",
                ],
                0,
            )
            .stdout;
            assert!(
                indexed
                    .split(|byte| *byte == 0)
                    .any(|path| path == name.as_bytes())
            );
            cli(root, &["detach"], 0);
            assert!(!root.join(name).exists());
            cli(root, &["attach"], 0);
            assert_eq!(fs::read(root.join(name)).unwrap(), b"private\n");
            fs::write(root.join(name), b"edited private\n").unwrap();
            cli(root, &["commit", "-m", "edited"], 0);
            cli(root, &["detach"], 0);
            cli(root, &["attach"], 0);
            assert_eq!(fs::read(root.join(name)).unwrap(), b"edited private\n");
            run(root, "git", &["add", "--all"], 0);
            assert!(
                run(root, "git", &["diff", "--cached", "--name-only"], 0)
                    .stdout
                    .is_empty()
            );
            cli(root, &["doctor"], 0);
        }
    }
    #[test]
    fn canonical_base_alias_is_refused_before_ownership_changes() {
        for precompose in ["true", "false"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            init(root, precompose);
            fs::write(root.join("é"), b"base\n").unwrap();
            run(root, "git", &["add", "é"], 0);
            run(root, "git", &["commit", "-m", "base file"], 0);
            let before = state(root);
            fs::write(root.join("e\u{301}"), b"private edit\n").unwrap();
            cli(root, &["add", "e\u{301}"], 1);
            assert_eq!(state(root), before);
            assert_eq!(fs::read(root.join("e\u{301}")).unwrap(), b"private edit\n");
        }
    }
    #[test]
    fn manifest_rejects_canonical_case_and_ancestor_aliases() {
        for names in [
            ["é", "e\u{301}"],
            ["É", "e\u{301}"],
            ["É", "e\u{301}/child"],
            ["Parent", "parent/child"],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            init(root, "false");
            let entries = names
                .into_iter()
                .map(|name| (name, serde_json::json!({"kind":"add","base":null})))
                .collect::<std::collections::BTreeMap<_, _>>();
            let manifest =
                serde_json::to_vec(&serde_json::json!({"version":2,"entries":entries})).unwrap();
            fs::write(root.join(".overgit/manifest.json"), &manifest).unwrap();
            cli(root, &["list"], 1);
            assert_eq!(
                fs::read(root.join(".overgit/manifest.json")).unwrap(),
                manifest
            );
        }
    }

    #[test]
    fn decomposed_paths_survive_local_clone_pull_and_sibling_exclusions() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path();
        init(root, "true");
        let name = "e\u{301}";
        fs::write(root.join(name), b"initial private\n").unwrap();
        cli(root, &["add", name], 0);
        cli(root, &["commit", "-m", "initial"], 0);
        let destination = tempfile::tempdir().unwrap();
        let copy = destination.path();
        run(
            root,
            "git",
            &["clone", root.to_str().unwrap(), copy.to_str().unwrap()],
            0,
        );
        let overlay = root.join(".overgit/.git");
        cli(copy, &["clone", overlay.to_str().unwrap()], 0);
        assert_eq!(fs::read(copy.join(name)).unwrap(), b"initial private\n");
        fs::write(root.join(name), b"updated private\n").unwrap();
        cli(root, &["commit", "-m", "updated"], 0);
        cli(copy, &["pull", "--ff-only"], 0);
        assert_eq!(fs::read(copy.join(name)).unwrap(), b"updated private\n");
        run(copy, "git", &["add", "--all"], 0);
        assert!(
            run(copy, "git", &["diff", "--cached", "--name-only"], 0)
                .stdout
                .is_empty()
        );
        let sibling_temp = tempfile::tempdir().unwrap();
        let sibling = sibling_temp.path().join("linked");
        cli(
            copy,
            &["worktree", "add", sibling.to_str().unwrap(), "-b", "linked"],
            0,
        );
        cli(copy, &["detach"], 0);
        run(&sibling, "git", &["add", "--all"], 0);
        assert!(
            run(&sibling, "git", &["diff", "--cached", "--name-only"], 0)
                .stdout
                .is_empty()
        );
        cli(copy, &["attach"], 0);
    }

    #[test]
    fn normalization_variants_are_reserved_in_base_exclusions() {
        for precompose in ["true", "false"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            init(root, precompose);
            fs::write(root.join("e\u{301}"), b"private\n").unwrap();
            // On normalization-insensitive filesystems, an NFC argument may name an NFD entry.
            let spelling = if root.join("é").exists() {
                "é"
            } else {
                "e\u{301}"
            };
            cli(root, &["add", spelling], 0);
            run(root, "git", &["add", "--all"], 0);
            assert!(
                run(root, "git", &["diff", "--cached", "--name-only"], 0)
                    .stdout
                    .is_empty()
            );
            for alias in ["é", "e\u{301}"] {
                run(root, "git", &["check-ignore", "--no-index", alias], 0);
            }
        }
    }

    #[test]
    fn exact_decomposed_base_and_canonical_gitlink_ancestors_are_protected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        init(root, "true");
        let name = "e\u{301}";
        fs::write(root.join(name), b"base bytes\n").unwrap();
        run(
            root,
            "git",
            &["-c", "core.precomposeunicode=false", "add", name],
            0,
        );
        run(root, "git", &["commit", "-m", "decomposed base"], 0);
        fs::write(root.join(name), b"private override\n").unwrap();
        cli(root, &["add", name], 0);
        cli(root, &["detach"], 0);
        assert_eq!(fs::read(root.join(name)).unwrap(), b"base bytes\n");
        cli(root, &["attach"], 0);
        assert_eq!(fs::read(root.join(name)).unwrap(), b"private override\n");
        run(root, "git", &["add", "--all"], 0);
        assert!(
            run(root, "git", &["diff", "--cached", "--name-only"], 0)
                .stdout
                .is_empty()
        );
        let head = String::from_utf8(run(root, "git", &["rev-parse", "HEAD"], 0).stdout).unwrap();
        run(
            root,
            "git",
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                head.trim(),
                "É-module",
            ],
            0,
        );
        fs::create_dir(root.join("e\u{301}-module")).unwrap();
        fs::write(root.join("e\u{301}-module/child"), b"submodule content\n").unwrap();
        let before = state(root);
        cli(root, &["add", "e\u{301}-module/child"], 1);
        assert_eq!(state(root), before);
    }
    #[test]
    fn existing_distinct_unowned_normalized_sibling_is_not_hidden() {
        use std::os::unix::fs::MetadataExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        init(root, "false");
        fs::write(root.join("é"), b"unowned sibling\n").unwrap();
        fs::write(root.join("e\u{301}"), b"requested overlay\n").unwrap();
        if fs::metadata(root.join("é")).unwrap().ino()
            == fs::metadata(root.join("e\u{301}")).unwrap().ino()
        {
            return; // This filesystem cannot represent distinct canonical siblings.
        }
        let before = state(root);
        cli(root, &["add", "e\u{301}"], 1);
        assert_eq!(state(root), before);
        assert_eq!(fs::read(root.join("é")).unwrap(), b"unowned sibling\n");
        assert_eq!(
            fs::read(root.join("e\u{301}")).unwrap(),
            b"requested overlay\n"
        );
        run(root, "git", &["check-ignore", "--no-index", "é"], 1);
        run(root, "git", &["check-ignore", "--no-index", "e\u{301}"], 1);
        fs::remove_file(root.join("e\u{301}")).unwrap();
        fs::hard_link(root.join("é"), root.join("e\u{301}")).unwrap();
        cli(root, &["add", "e\u{301}"], 1);
        assert_eq!(state(root), before);
        fs::create_dir(root.join("édir")).unwrap();
        fs::create_dir(root.join("e\u{301}dir")).unwrap();
        fs::write(root.join("édir/file"), b"unowned nested sibling\n").unwrap();
        fs::hard_link(root.join("édir/file"), root.join("e\u{301}dir/file")).unwrap();
        cli(root, &["add", "e\u{301}dir/file"], 1);
        assert_eq!(state(root), before);
    }
}
