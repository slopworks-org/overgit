#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    fn run(root: &Path, program: &str, args: &[&str]) {
        let output = Command::new(program)
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Path Test")
            .env("GIT_AUTHOR_EMAIL", "path@example.invalid")
            .env("GIT_COMMITTER_NAME", "Path Test")
            .env("GIT_COMMITTER_EMAIL", "path@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn trailing_newline_root_never_retargets_a_sibling_repository() {
        let temporary = tempfile::tempdir().unwrap();
        let sibling = temporary.path().join("repo");
        let root = temporary.path().join("repo\n\n");
        for directory in [&sibling, &root] {
            fs::create_dir(directory).unwrap();
            run(directory, "git", &["init", "--initial-branch=main"]);
            fs::write(directory.join("tracked"), b"base\n").unwrap();
            run(directory, "git", &["add", "tracked"]);
            run(directory, "git", &["commit", "-m", "base"]);
        }
        let original_index = fs::read(sibling.join(".git/index")).unwrap();
        let original_exclude = fs::read(sibling.join(".git/info/exclude")).unwrap();
        let binary = env!("CARGO_BIN_EXE_overgit");
        run(&root, binary, &["init"]);
        assert!(root.join(".overgit/.git").is_dir());
        assert!(!sibling.join(".overgit").exists());
        fs::write(root.join("tracked"), b"overlay\n").unwrap();
        run(&root, binary, &["add", "tracked"]);
        run(&root, binary, &["detach"]);
        assert_eq!(fs::read(root.join("tracked")).unwrap(), b"base\n");
        run(&root, binary, &["attach"]);
        assert_eq!(fs::read(root.join("tracked")).unwrap(), b"overlay\n");
        fs::write(root.join("private"), b"private\n").unwrap();
        run(&root, binary, &["add", "private"]);
        run(&root, binary, &["commit", "-m", "overlay"]);
        run(&root, "git", &["config", "core.hooksPath", "hooks\n"]);
        run(&root, binary, &["hooks", "install"]);
        fs::remove_file(root.join("private")).unwrap();
        run(&root, "git", &["checkout", "-b", "next"]);
        assert_eq!(fs::read(root.join("private")).unwrap(), b"private\n");
        let linked = temporary.path().join("linked\n");
        run(
            &root,
            binary,
            &["worktree", "add", linked.to_str().unwrap(), "-b", "linked"],
        );
        run(&linked, binary, &["hooks", "install"]);
        fs::remove_file(linked.join("private")).unwrap();
        run(&linked, "git", &["checkout", "-b", "linked-next"]);
        assert_eq!(fs::read(linked.join("private")).unwrap(), b"private\n");
        assert_eq!(fs::read(sibling.join("tracked")).unwrap(), b"base\n");
        assert_eq!(
            fs::read(sibling.join(".git/index")).unwrap(),
            original_index
        );
        assert_eq!(
            fs::read(sibling.join(".git/info/exclude")).unwrap(),
            original_exclude
        );
    }
}
