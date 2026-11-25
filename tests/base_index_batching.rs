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

    fn real_git() -> PathBuf {
        let output = Command::new("which").arg("git").output().unwrap();
        assert!(output.status.success());
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    }

    fn install_git_wrapper(dir: &Path, body: &str) -> PathBuf {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let wrapper = bin.join("git");
        fs::write(&wrapper, body).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn prepended_path(bin: &Path) -> String {
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    #[test]
    fn batched_base_index_updates_handle_many_files_and_special_names() {
        let sandbox = Sandbox::new();
        let mut names: Vec<String> = (0..18).map(|i| format!("file_{i:02}.txt")).collect();
        names.push("with space.txt".into());
        names.push("-leading-dash.txt".into());
        for name in &names {
            sandbox.write(name, b"base\n");
        }
        sandbox.git(&["add", "--all"]);
        sandbox.git(&["commit", "-m", "many files"]);
        for name in &names {
            sandbox.write(name, b"overlay\n");
        }
        let mut add_args = vec!["add", "--"];
        add_args.extend(names.iter().map(String::as_str));
        sandbox.cli(&add_args, 0);
        sandbox.cli(&["commit", "-m", "own many"], 0);

        let log = sandbox.temporary.path().join("update-index.log");
        let bin = install_git_wrapper(
            sandbox.temporary.path(),
            "#!/bin/sh\nif [ -z \"$GIT_DIR\" ]; then\n  case \"$*\" in\n    *\"update-index\"*) echo \"$*\" >> \"$TEST_LOG\" ;;\n  esac\nfi\nexec \"$TEST_REAL_GIT\" \"$@\"\n",
        );

        let detach = sandbox
            .command(env!("CARGO_BIN_EXE_overgit"))
            .env("PATH", prepended_path(&bin))
            .env("TEST_REAL_GIT", real_git())
            .env("TEST_LOG", &log)
            .args(["detach"])
            .output()
            .unwrap();
        assert!(
            detach.status.success(),
            "detach failed: {}",
            String::from_utf8_lossy(&detach.stderr)
        );
        let detach_calls = fs::read_to_string(&log).unwrap();
        assert_eq!(
            detach_calls.lines().count(),
            1,
            "expected 1 batched base update-index call on detach, got:\n{detach_calls}"
        );
        let flags_detached = String::from_utf8(sandbox.git(&["ls-files", "-v"])).unwrap();
        for name in &names {
            assert!(
                flags_detached
                    .lines()
                    .any(|line| line == format!("H {name}")),
                "expected cleared skip-worktree for {name:?} in:\n{flags_detached}"
            );
        }

        fs::write(&log, b"").unwrap();
        let attach = sandbox
            .command(env!("CARGO_BIN_EXE_overgit"))
            .env("PATH", prepended_path(&bin))
            .env("TEST_REAL_GIT", real_git())
            .env("TEST_LOG", &log)
            .args(["attach"])
            .output()
            .unwrap();
        assert!(
            attach.status.success(),
            "attach failed: {}",
            String::from_utf8_lossy(&attach.stderr)
        );
        let attach_calls = fs::read_to_string(&log).unwrap();
        assert_eq!(
            attach_calls.lines().count(),
            1,
            "expected 1 batched base update-index call on attach, got:\n{attach_calls}"
        );
        let flags_attached = String::from_utf8(sandbox.git(&["ls-files", "-v"])).unwrap();
        for name in &names {
            assert!(
                flags_attached
                    .lines()
                    .any(|line| line == format!("S {name}")),
                "expected skip-worktree for {name:?} in:\n{flags_attached}"
            );
        }
    }

    #[test]
    fn transient_base_index_lock_contention_is_retried() {
        let sandbox = Sandbox::new();
        sandbox.write("f", b"overlay\n");
        sandbox.cli(&["add", "f"], 0);
        sandbox.cli(&["commit", "-m", "own f"], 0);

        let marker = sandbox.temporary.path().join("failed-once");
        let bin = install_git_wrapper(
            sandbox.temporary.path(),
            "#!/bin/sh\nif [ -z \"$GIT_DIR\" ]; then\n  case \"$*\" in\n    *\"update-index\"*)\n      if [ ! -e \"$TEST_MARKER\" ]; then\n        : > \"$TEST_MARKER\"\n        echo \"fatal: Unable to create '$PWD/.git/index.lock': File exists.\" >&2\n        exit 128\n      fi\n      ;;\n  esac\nfi\nexec \"$TEST_REAL_GIT\" \"$@\"\n",
        );

        let output = sandbox
            .command(env!("CARGO_BIN_EXE_overgit"))
            .env("PATH", prepended_path(&bin))
            .env("TEST_REAL_GIT", real_git())
            .env("TEST_MARKER", &marker)
            .args(["detach"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "detach should retry transient index.lock contention: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(marker.exists());
        assert!(sandbox.root.join(".overgit/local/detached.json").exists());
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"base\n");
    }

    #[test]
    fn transient_index_lock_retries_even_when_large_stdin_hits_broken_pipe() {
        let sandbox = Sandbox::new();
        let long_dir = "a".repeat(80);
        // > 80 KB of NUL-delimited paths guarantees BrokenPipe if git exits before reading stdin.
        let names: Vec<String> = (0..900)
            .map(|i| format!("{long_dir}/file_{i:04}.txt"))
            .collect();
        for name in &names {
            sandbox.write(name, b"b\n");
        }
        sandbox.git(&["add", "--all"]);
        sandbox.git(&["commit", "-m", "many long paths"]);
        sandbox.cli(&["rm", &long_dir], 0);
        sandbox.cli(&["commit", "-m", "hide long dir"], 0);

        let marker = sandbox.temporary.path().join("failed-once-large");
        let bin = install_git_wrapper(
            sandbox.temporary.path(),
            "#!/bin/sh\nif [ -z \"$GIT_DIR\" ]; then\n  case \"$*\" in\n    *\"update-index\"*)\n      if [ ! -e \"$TEST_MARKER\" ]; then\n        : > \"$TEST_MARKER\"\n        echo \"fatal: Unable to create '$PWD/.git/index.lock': File exists.\" >&2\n        exit 128\n      fi\n      ;;\n  esac\nfi\nexec \"$TEST_REAL_GIT\" \"$@\"\n",
        );

        let output = sandbox
            .command(env!("CARGO_BIN_EXE_overgit"))
            .env("PATH", prepended_path(&bin))
            .env("TEST_REAL_GIT", real_git())
            .env("TEST_MARKER", &marker)
            .args(["detach"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "detach with >64KB stdin should retry after BrokenPipe: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(marker.exists());
        assert!(sandbox.root.join(".overgit/local/detached.json").exists());
    }

    #[test]
    fn detach_allows_pristine_base_whiteout_and_rejects_modified_whiteout() {
        let sandbox = Sandbox::new();
        sandbox.cli(&["rm", "f"], 0);
        sandbox.cli(&["commit", "-m", "hide f"], 0);

        // Recreate exact base bytes on disk for a whiteout path.
        sandbox.write("f", b"base\n");
        sandbox.cli(&["detach"], 0);
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"base\n");

        sandbox.cli(&["attach"], 0);
        assert!(!sandbox.root.join("f").exists());

        // Unexpected modified content still requires --force and is rescued.
        sandbox.write("f", b"unsaved draft\n");
        let rejected = sandbox.cli(&["detach"], 1);
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("unexpected content"),
            "stderr: {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
        sandbox.cli(&["detach", "--force"], 0);
        assert_eq!(fs::read(sandbox.root.join("f")).unwrap(), b"base\n");
    }
}
