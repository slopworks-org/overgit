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
    fn removal_preserves_pending_pull_and_aborted_recovery_backups() {
        let pair = Pair::new();
        let child = pair.temporary.path().join("child");
        cli(
            &pair.destination,
            &["worktree", "add", child.to_str().unwrap(), "-b", "topic"],
            0,
        );
        Pair::commit(&pair.source, b"upstream\n", "upstream");
        Pair::commit(&child, b"local\n", "local");
        cli(
            &child,
            &[
                "pull",
                "--no-rebase",
                pair.source.join(".overgit/.git").to_str().unwrap(),
                "HEAD",
            ],
            3,
        );
        let unique = b"unique pending pull resolution\n";
        let file = child.join(".overgit/local/pull/tree/only-copy");
        fs::write(&file, unique).unwrap();
        let remove = ["worktree", "remove", child.to_str().unwrap()];
        cli(&pair.destination, &remove, 1);
        assert_eq!(fs::read(&file).unwrap(), unique);
        cli(&child, &["pull", "--abort"], 0);
        let backups = child.join(".overgit/local/backups");
        assert!(contains_bytes(&backups, unique));
        cli(&pair.destination, &remove, 1);
        assert!(contains_bytes(&backups, unique));
        cli(
            &pair.destination,
            &["worktree", "remove", "--force", child.to_str().unwrap()],
            0,
        );
        assert!(!child.exists());
    }
    #[test]
    fn removal_refuses_an_interrupted_mutation_even_with_clean_indexes() {
        let pair = Pair::new();
        let child = pair.temporary.path().join("child");
        cli(
            &pair.destination,
            &["worktree", "add", child.to_str().unwrap(), "-b", "topic"],
            0,
        );
        let journal = child.join(".overgit/local/mutation.json");
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(&journal, b"interrupted journal bytes").unwrap();
        cli(
            &pair.destination,
            &["worktree", "remove", child.to_str().unwrap()],
            1,
        );
        assert_eq!(fs::read(&journal).unwrap(), b"interrupted journal bytes");
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
