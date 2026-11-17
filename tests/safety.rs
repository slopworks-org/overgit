#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::symlink,
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
            fs::write(sandbox.root.join("victim"), b"pristine base\n").unwrap();
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
                .env("XDG_CONFIG_HOME", self.temporary.path().join("config"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Safety Test")
                .env("GIT_AUTHOR_EMAIL", "safety@example.invalid")
                .env("GIT_COMMITTER_NAME", "Safety Test")
                .env("GIT_COMMITTER_EMAIL", "safety@example.invalid")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "protocol.allow")
                .env("GIT_CONFIG_VALUE_0", "never");
            command
        }

        fn checked(&self, executable: &str, arguments: &[&str], code: i32) -> Output {
            let output = self.command(executable).args(arguments).output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(code),
                "{executable} {arguments:?}\nstdout {}\nstderr {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        fn git(&self, arguments: &[&str]) -> Output {
            self.checked("git", arguments, 0)
        }

        fn cli(&self, arguments: &[&str], code: i32) -> Output {
            self.checked(env!("CARGO_BIN_EXE_overgit"), arguments, code)
        }

        fn backups(&self, path: &[u8]) -> Vec<Vec<u8>> {
            fs::read_dir(self.root.join(".overgit/local/backups"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|directory| fs::read(directory.join("path")).unwrap() == path)
                .map(|directory| fs::read(directory.join("content")).unwrap())
                .collect()
        }
    }

    #[test]
    fn root_pointing_leaf_symlink_is_owned_and_removed_as_one_file() {
        let sandbox = Sandbox::new();
        symlink(".", sandbox.root.join("again")).unwrap();
        sandbox.cli(&["add", "again"], 0);
        assert_eq!(
            sandbox.cli(&["list", "--porcelain"], 0).stdout,
            b"add\tagain\0"
        );
        assert_eq!(
            fs::read_link(sandbox.root.join("again")).unwrap(),
            PathBuf::from(".")
        );
        sandbox.cli(&["rm", "again"], 0);
        assert!(sandbox.cli(&["list", "--porcelain"], 0).stdout.is_empty());
        assert_eq!(
            fs::read(sandbox.root.join("victim")).unwrap(),
            b"pristine base\n"
        );
        assert!(
            sandbox
                .git(&["status", "--porcelain", "--untracked-files=all"])
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn symlink_before_parent_component_cannot_retarget_an_in_root_victim() {
        let sandbox = Sandbox::new();
        let outside = sandbox.temporary.path().join("outside");
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(outside.join("victim"), b"outside bytes\n").unwrap();
        symlink(outside.join("nested"), sandbox.root.join("link")).unwrap();
        sandbox.cli(&["add", "link/../victim"], 1);
        sandbox.cli(&["rm", "link/../victim"], 1);
        assert!(sandbox.cli(&["list", "--porcelain"], 0).stdout.is_empty());
        assert_eq!(
            fs::read(sandbox.root.join("victim")).unwrap(),
            b"pristine base\n"
        );
        assert_eq!(
            fs::read(outside.join("victim")).unwrap(),
            b"outside bytes\n"
        );
        assert!(
            sandbox
                .git(&["diff", "--cached", "--name-only"])
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn removing_a_staged_only_add_keeps_its_bytes_in_a_named_backup() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("secret"), b"never committed\n").unwrap();
        sandbox.cli(&["add", "secret"], 0);
        sandbox.cli(&["rm", "secret"], 0);
        assert!(!sandbox.root.join("secret").exists());
        assert!(
            sandbox
                .backups(b"secret")
                .contains(&b"never committed\n".to_vec())
        );
        assert!(sandbox.cli(&["list", "--porcelain"], 0).stdout.is_empty());
    }

    #[test]
    fn taking_upstream_rescues_uncommitted_pre_sync_bytes_after_a_hand_edit() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("victim"), b"overlay committed\n").unwrap();
        sandbox.cli(&["add", "victim"], 0);
        sandbox.cli(&["commit", "-m", "overlay"], 0);
        sandbox.cli(&["detach"], 0);
        fs::write(sandbox.root.join("victim"), b"upstream\n").unwrap();
        sandbox.git(&["add", "victim"]);
        sandbox.git(&["commit", "-m", "upstream"]);
        sandbox.cli(&["attach"], 0);
        fs::write(sandbox.root.join("victim"), b"uncommitted pre-sync\n").unwrap();
        sandbox.cli(&["sync"], 3);
        fs::write(sandbox.root.join("victim"), b"upstream\n").unwrap();
        sandbox.cli(&["resolve", "--take-upstream", "victim"], 0);
        assert!(
            sandbox
                .backups(b"victim")
                .contains(&b"uncommitted pre-sync\n".to_vec())
        );
        assert_eq!(
            fs::read(sandbox.root.join("victim")).unwrap(),
            b"upstream\n"
        );
        assert!(
            sandbox
                .git(&["status", "--porcelain", "--untracked-files=all"])
                .stdout
                .is_empty()
        );
    }
    #[test]
    fn doctor_recovers_missing_overlay_index_content_and_metadata_excludes() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.root.join("victim"), b"saved overlay\n").unwrap();
        sandbox.cli(&["add", "victim"], 0);
        sandbox.cli(&["commit", "-m", "saved"], 0);
        sandbox.cli(
            &["git", "update-index", "--force-remove", "--", "victim"],
            0,
        );
        fs::write(sandbox.root.join("victim"), b"ordinary edit\n").unwrap();
        fs::write(
            sandbox.root.join(".overgit/.git/info/exclude"),
            b"# custom\n",
        )
        .unwrap();
        sandbox.cli(&["doctor"], 4);
        sandbox.cli(&["doctor", "--fix"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("victim")).unwrap(),
            b"ordinary edit\n"
        );
        assert_eq!(
            sandbox.cli(&["git", "show", ":victim"], 0).stdout,
            b"saved overlay\n"
        );
        let exclude = fs::read_to_string(sandbox.root.join(".overgit/.git/info/exclude")).unwrap();
        assert!(exclude.contains("# custom"));
        assert!(exclude.contains("/.overgit/local/"));
    }
}
