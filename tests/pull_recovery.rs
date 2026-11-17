#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{Command, Output},
    };
    use tempfile::TempDir;

    struct Pair {
        temporary: TempDir,
        source: PathBuf,
        destination: PathBuf,
    }
    fn command(executable: &str, cwd: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Audit Pull")
            .env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
            .env("GIT_COMMITTER_NAME", "Audit Pull")
            .env("GIT_COMMITTER_EMAIL", "audit@example.invalid")
            .env("GIT_EDITOR", "true")
            .env("GIT_PAGER", "cat");
        command
    }
    fn run(executable: &str, cwd: &Path, args: &[&str], code: i32) -> Output {
        let output = command(executable, cwd).args(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{args:?}\nstdout {}\nstderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
        run("git", cwd, args, 0).stdout
    }
    fn cli(cwd: &Path, args: &[&str], code: i32) -> Output {
        run(env!("CARGO_BIN_EXE_overgit"), cwd, args, code)
    }
    impl Pair {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source");
            let destination = temporary.path().join("destination");
            fs::create_dir(&source).unwrap();
            git(&source, &["init", "--initial-branch=main"]);
            fs::write(source.join("f"), b"base\n").unwrap();
            fs::write(source.join("unrelated"), b"base only\n").unwrap();
            git(&source, &["add", "f", "unrelated"]);
            git(&source, &["commit", "-m", "base"]);
            cli(&source, &["init"], 0);
            fs::write(source.join("f"), b"common overlay\n").unwrap();
            cli(&source, &["add", "f"], 0);
            cli(&source, &["commit", "-m", "common overlay"], 0);
            git(
                temporary.path(),
                &[
                    "clone",
                    source.to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            );
            cli(
                &destination,
                &["clone", source.join(".overgit/.git").to_str().unwrap()],
                0,
            );
            Self {
                temporary,
                source,
                destination,
            }
        }
        fn commit(root: &Path, data: &[u8], message: &str) {
            fs::write(root.join("f"), data).unwrap();
            cli(root, &["commit", "-m", message], 0);
        }
    }
    #[test]
    fn interrupted_pull_rescues_staged_bytes_and_preserves_current_excludes() {
        use std::os::unix::{fs::PermissionsExt as _, process::ExitStatusExt as _};
        let pair = Pair::new();
        Pair::commit(&pair.source, b"upstream\n", "upstream");
        cli(
            &pair.destination,
            &["git", "update-index", "--split-index"],
            0,
        );
        git(&pair.destination, &["update-index", "--split-index"]);
        let real_git =
            String::from_utf8(run("sh", &pair.destination, &["-c", "command -v git"], 0).stdout)
                .unwrap();
        let binaries = pair.temporary.path().join("bin");
        fs::create_dir(&binaries).unwrap();
        let wrapper = binaries.join("git");
        fs::write(&wrapper, b"#!/bin/sh\ncase \"$*\" in\n *\"read-tree \"*) \"$AUDIT_REAL_GIT\" \"$@\" || exit $?; kill -KILL \"$PPID\"; exit 0;;\nesac\nexec \"$AUDIT_REAL_GIT\" \"$@\"\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let killed = command(env!("CARGO_BIN_EXE_overgit"), &pair.destination)
            .args(["pull", "--ff-only"])
            .env("AUDIT_REAL_GIT", real_git.trim())
            .env(
                "PATH",
                format!("{}:{}", binaries.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert_eq!(killed.status.signal(), Some(9));
        assert!(
            pair.destination
                .join(".overgit/local/pull/install.json")
                .exists()
        );
        cli(
            &pair.destination,
            &["git", "update-index", "--no-split-index"],
            0,
        );
        git(&pair.destination, &["update-index", "--no-split-index"]);
        remove_shared_indexes(&pair.destination);
        git(
            &pair.destination,
            &["update-index", "--assume-unchanged", "unrelated"],
        );
        git(
            &pair.destination,
            &["update-index", "--skip-worktree", "unrelated"],
        );
        assert_eq!(
            git(&pair.destination, &["ls-files", "-v", "unrelated"]),
            b"s unrelated\n"
        );
        let staged = b"unique staged after interrupted pull\n";
        fs::write(pair.destination.join("f"), staged).unwrap();
        cli(&pair.destination, &["git", "add", "f"], 0);
        fs::write(pair.destination.join("f"), b"common overlay\n").unwrap();
        let exclude = pair.destination.join(".git/info/exclude");
        let mut rules = fs::read(&exclude).unwrap();
        rules.extend_from_slice(b"\nmanual-protection-after-crash\n");
        fs::write(&exclude, rules).unwrap();
        let edited_manifest = edit_manifest(&pair.destination);
        let lock = pair.destination.join(".git/index.lock");
        fs::write(&lock, b"external lock").unwrap();
        cli(&pair.destination, &["pull", "--abort"], 1);
        assert_eq!(
            git(&pair.destination, &["ls-files", "-v", "unrelated"]),
            b"s unrelated\n"
        );
        fs::remove_file(lock).unwrap();
        cli(&pair.destination, &["pull", "--abort"], 0);
        cli(&pair.destination, &["git", "ls-files", "--stage"], 0);
        assert_eq!(
            git(&pair.destination, &["ls-files", "-v", "unrelated"]),
            b"s unrelated\n"
        );
        cli(
            &pair.destination,
            &["git", "reflog", "expire", "--expire=now", "--all"],
            0,
        );
        cli(&pair.destination, &["git", "gc", "--prune=now"], 0);
        assert!(contains_bytes(
            &pair.destination.join(".overgit/local/backups"),
            &edited_manifest
        ));
        assert!(contains_bytes(
            &pair.destination.join(".overgit/local/backups"),
            staged
        ));
        assert!(
            fs::read_to_string(exclude)
                .unwrap()
                .contains("manual-protection-after-crash")
        );
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"common overlay\n"
        );
    }
    fn edit_manifest(root: &Path) -> Vec<u8> {
        use base64::Engine as _;
        let path = root.join(".overgit/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["entries"]["only-unstaged-whiteout"] = serde_json::json!({
            "kind": "delete", "base": {"mode": "100644", "data": base64::engine::general_purpose::STANDARD.encode(b"unique unstaged fork")}
        });
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(path, &bytes).unwrap();
        bytes
    }
    fn remove_shared_indexes(root: &Path) {
        for directory in [".git", ".overgit/.git"] {
            for entry in fs::read_dir(root.join(directory)).unwrap() {
                let entry = entry.unwrap();
                if entry
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with("sharedindex.")
                {
                    fs::remove_file(entry.path()).unwrap();
                }
            }
        }
    }
    fn contains_bytes(directory: &Path, bytes: &[u8]) -> bool {
        fs::read_dir(directory).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                contains_bytes(&path, bytes)
            } else {
                fs::read(path).unwrap() == bytes
            }
        })
    }
}
