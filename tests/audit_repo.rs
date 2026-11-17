#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        io::Write as _,
        os::unix::{
            ffi::OsStringExt as _,
            fs::{PermissionsExt as _, symlink},
        },
        path::{Path, PathBuf},
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
            fs::write(sandbox.root.join("tracked.txt"), b"base\n").unwrap();
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
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Audit Test")
                .env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
                .env("GIT_COMMITTER_NAME", "Audit Test")
                .env("GIT_COMMITTER_EMAIL", "audit@example.invalid")
                .env("GIT_EDITOR", "true")
                .env("GIT_PAGER", "cat");
            command
        }
        fn checked(&self, executable: &str, args: &[&str], code: i32) -> Output {
            let output = self.command(executable).args(args).output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(code),
                "{args:?}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }
        fn git(&self, args: &[&str]) -> Output {
            self.checked("git", args, 0)
        }
        fn cli(&self, args: &[&str], code: i32) -> Output {
            self.checked(env!("CARGO_BIN_EXE_overgit"), args, code)
        }
        fn own(&self) {
            fs::write(self.root.join("tracked.txt"), b"overlay\n").unwrap();
            self.cli(&["add", "tracked.txt"], 0);
            self.cli(&["commit", "-m", "overlay"], 0);
        }
    }

    fn redirect_directory(sandbox: &Sandbox, relative: &str, name: &str) -> PathBuf {
        let source = sandbox.root.join(relative);
        let outside = sandbox.temporary.path().join(name);
        fs::rename(&source, &outside).unwrap();
        symlink(&outside, &source).unwrap();
        outside
    }

    #[test]
    fn base_info_parent_symlink_is_rejected_before_any_external_write() {
        let sandbox = Sandbox::new();
        sandbox.own();
        let outside = redirect_directory(&sandbox, ".git/info", "base-info");
        let before = fs::read(outside.join("exclude")).unwrap();
        sandbox.cli(&["apply"], 1);
        assert_eq!(fs::read(outside.join("exclude")).unwrap(), before);
        assert_eq!(
            fs::read(sandbox.root.join("tracked.txt")).unwrap(),
            b"overlay\n"
        );
    }

    #[test]
    fn overlay_info_parent_symlink_is_rejected_before_doctor_fix() {
        let sandbox = Sandbox::new();
        let outside = redirect_directory(&sandbox, ".overgit/.git/info", "overlay-info");
        fs::write(outside.join("exclude"), b"# external sentinel\n").unwrap();
        sandbox.cli(&["doctor", "--fix"], 1);
        assert_eq!(
            fs::read(outside.join("exclude")).unwrap(),
            b"# external sentinel\n"
        );
    }

    #[test]
    fn git_object_parent_symlink_cannot_redirect_owned_content() {
        let sandbox = Sandbox::new();
        let outside = redirect_directory(&sandbox, ".overgit/.git/objects", "overlay-objects");
        let before = directory_entries(&outside);
        fs::write(sandbox.root.join("private"), b"never outside\n").unwrap();
        sandbox.cli(&["add", "private"], 1);
        assert_eq!(directory_entries(&outside), before);
        assert_eq!(
            fs::read(sandbox.root.join("private")).unwrap(),
            b"never outside\n"
        );
    }

    fn directory_entries(path: &Path) -> Vec<PathBuf> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            entries.push(entry.path());
            if entry.file_type().unwrap().is_dir() {
                entries.extend(directory_entries(&entry.path()));
            }
        }
        entries.sort();
        entries
    }

    #[test]
    fn custom_hook_parent_symlink_cannot_redirect_hook_installation() {
        let sandbox = Sandbox::new();
        let outside = sandbox.temporary.path().join("outside-hooks");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(sandbox.root.join("settings")).unwrap();
        symlink(&outside, sandbox.root.join("settings/redirect")).unwrap();
        sandbox.git(&["config", "core.hooksPath", "settings/redirect/hooks"]);
        sandbox.cli(&["hooks", "install"], 1);
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn unrelated_non_utf8_base_path_does_not_block_utf8_ownership() {
        let sandbox = Sandbox::new();
        let oid = sandbox.git(&["rev-parse", "HEAD:tracked.txt"]).stdout;
        let mut record =
            format!("100644 {}\t", String::from_utf8(oid).unwrap().trim()).into_bytes();
        record.extend_from_slice(b"raw-\xff\0");
        let mut child = sandbox
            .command("git")
            .args(["update-index", "-z", "--index-info"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&record).unwrap();
        assert!(child.wait().unwrap().success());
        let raw = OsString::from_vec(b"raw-\xff".to_vec());
        let hidden = sandbox
            .command("git")
            .args(["update-index", "--skip-worktree", "--"])
            .arg(&raw)
            .output()
            .unwrap();
        assert!(
            hidden.status.success(),
            "{}",
            String::from_utf8_lossy(&hidden.stderr)
        );
        sandbox.git(&["commit", "-m", "raw name"]);
        sandbox.own();
        sandbox.cli(&["detach"], 0);
        assert_eq!(
            fs::read(sandbox.root.join("tracked.txt")).unwrap(),
            b"base\n"
        );
        sandbox.cli(&["attach"], 0);
        sandbox.git(&["add", "--all"]);
        assert!(
            sandbox
                .git(&["diff", "--cached", "--name-only", "-z"])
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn passthrough_preserves_git_quiet_color_and_wildcard_options() {
        let sandbox = Sandbox::new();
        sandbox.own();
        fs::write(sandbox.root.join("tracked.txt"), b"modified\n").unwrap();
        let quiet = sandbox.cli(&["diff", "--quiet"], 1);
        assert!(quiet.stdout.is_empty());
        let wildcard = sandbox.cli(&["diff", "--", "*.txt"], 0);
        assert!(String::from_utf8_lossy(&wildcard.stdout).contains("modified"));
        let log = sandbox.cli(&["log", "--color=always", "--oneline", "-1"], 0);
        assert!(log.stdout.contains(&0x1b));
    }

    #[test]
    fn passthrough_inherits_user_prompt_and_external_diff_environment() {
        let sandbox = Sandbox::new();
        sandbox.own();
        let wrapper_dir = sandbox.temporary.path().join("bin");
        fs::create_dir(&wrapper_dir).unwrap();
        let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|entry| entry.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let wrapper = wrapper_dir.join("git");
        fs::write(&wrapper, b"#!/bin/sh\nprintf '%s|%s|%s\\n' \"$*\" \"${GIT_TERMINAL_PROMPT-unset}\" \"${GIT_EXTERNAL_DIFF-unset}\" >> \"$WRAPPER_LOG\"\nexec \"$REAL_GIT\" \"$@\"\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let log = sandbox.temporary.path().join("git-calls");
        let path = std::env::join_paths(
            std::iter::once(wrapper_dir)
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let output = sandbox
            .command(env!("CARGO_BIN_EXE_overgit"))
            .args(["git", "status", "--porcelain"])
            .env("PATH", path)
            .env("REAL_GIT", real_git)
            .env("WRAPPER_LOG", &log)
            .env("GIT_EXTERNAL_DIFF", "audit-diff-helper")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = fs::read_to_string(log).unwrap();
        assert!(
            calls
                .lines()
                .any(|line| line.contains("status --porcelain|unset|audit-diff-helper")),
            "{calls}"
        );
        assert!(
            calls.lines().any(|line| line.ends_with("|0|unset")),
            "plumbing must have a distinct deterministic environment\n{calls}"
        );
    }
    #[test]
    fn file_and_directory_rescues_include_named_recovery_metadata() {
        let sandbox = Sandbox::new();
        sandbox.own();
        fs::write(sandbox.root.join("tracked.txt"), b"unexpected edit\n").unwrap();
        sandbox.cli(&["apply"], 0);
        fs::remove_file(sandbox.root.join("tracked.txt")).unwrap();
        fs::create_dir(sandbox.root.join("tracked.txt")).unwrap();
        fs::write(sandbox.root.join("tracked.txt/child"), b"directory edit\n").unwrap();
        sandbox.cli(&["apply", "--force"], 0);
        let mut types = Vec::new();
        for entry in fs::read_dir(sandbox.root.join(".overgit/local/backups")).unwrap() {
            let directory = entry.unwrap().path();
            let metadata: serde_json::Value =
                serde_json::from_slice(&fs::read(directory.join("recovery.json")).unwrap())
                    .unwrap();
            assert_eq!(metadata["original_path"], "tracked.txt");
            assert!(metadata["timestamp_unix"].as_u64().unwrap() > 0);
            assert!(!metadata["reason"].as_str().unwrap().is_empty());
            assert!(!metadata["original_mode"].as_str().unwrap().is_empty());
            assert_eq!(metadata["content_location"], "content");
            assert_eq!(fs::read(directory.join("path")).unwrap(), b"tracked.txt");
            types.push(metadata["original_type"].as_str().unwrap().to_owned());
            if metadata["original_type"] == "directory" {
                assert_eq!(
                    fs::read(directory.join("content/child")).unwrap(),
                    b"directory edit\n"
                );
            }
        }
        types.sort();
        assert_eq!(types, ["directory", "file"]);
    }
}
