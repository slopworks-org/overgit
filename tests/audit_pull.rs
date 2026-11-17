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
            git(&source, &["add", "f"]);
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
        fn diverge(&self) {
            Self::commit(&self.source, b"upstream overlay\n", "upstream");
            Self::commit(&self.destination, b"local overlay\n", "local");
        }
        fn index(&self) -> Vec<u8> {
            fs::read(self.destination.join(".overgit/.git/index")).unwrap()
        }
        fn head(&self) -> Vec<u8> {
            cli(&self.destination, &["git", "rev-parse", "HEAD"], 0).stdout
        }
    }
    #[test]
    fn pull_repository_argument_uses_invoking_directory_even_with_global_c() {
        let pair = Pair::new();
        Pair::commit(&pair.source, b"updated\n", "updated");
        cli(
            pair.temporary.path(),
            &[
                "-C",
                "destination",
                "pull",
                "--ff-only",
                "../source/.overgit/.git",
                "HEAD",
            ],
            0,
        );
        assert_eq!(fs::read(pair.destination.join("f")).unwrap(), b"updated\n");
    }
    #[test]
    fn conflicted_pull_can_be_resolved_in_persistent_isolation_and_continued() {
        let pair = Pair::new();
        pair.diverge();
        let head = pair.head();
        let index = pair.index();
        cli(&pair.destination, &["pull", "--no-rebase"], 3);
        assert_eq!(pair.head(), head);
        assert_eq!(pair.index(), index);
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"local overlay\n"
        );
        let checkout = pair.destination.join(".overgit/local/pull/tree");
        assert!(
            String::from_utf8_lossy(&fs::read(checkout.join("f")).unwrap()).contains("<<<<<<<")
        );
        cli(&pair.destination, &["pull", "--continue"], 3);
        fs::write(checkout.join("f"), b"resolved both versions\n").unwrap();
        cli(&pair.destination, &["pull", "--git", "add", "f"], 0);
        cli(&pair.destination, &["pull", "--continue"], 0);
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"resolved both versions\n"
        );
        assert!(!pair.destination.join(".overgit/local/pull").exists());
        assert!(git(&pair.destination, &["status", "--porcelain"]).is_empty());
    }
    #[test]
    fn pending_pull_refuses_to_overwrite_original_edits_and_abort_preserves_isolated_edits() {
        let pair = Pair::new();
        pair.diverge();
        let head = pair.head();
        let index = pair.index();
        cli(&pair.destination, &["pull", "--no-rebase"], 3);
        let checkout = pair.destination.join(".overgit/local/pull/tree");
        fs::write(checkout.join("f"), b"isolated edited resolution\n").unwrap();
        cli(&pair.destination, &["pull", "--git", "add", "f"], 0);
        fs::write(pair.destination.join("f"), b"new original edit\n").unwrap();
        cli(&pair.destination, &["pull", "--continue"], 1);
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"new original edit\n"
        );
        assert_eq!(pair.head(), head);
        assert_eq!(pair.index(), index);
        cli(&pair.destination, &["pull", "--abort"], 0);
        let backups = fs::read_dir(pair.destination.join(".overgit/local/backups")).unwrap();
        assert!(
            backups
                .map(|entry| entry.unwrap().path())
                .any(|path| fs::read(path.join("tree/f")).ok().as_deref()
                    == Some(b"isolated edited resolution\n"))
        );
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"new original edit\n"
        );
    }
    #[test]
    fn conflicted_rebase_can_be_continued_without_shared_conflict_files() {
        let pair = Pair::new();
        pair.diverge();
        let head = pair.head();
        cli(&pair.destination, &["pull", "--rebase"], 3);
        assert_eq!(pair.head(), head);
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"local overlay\n"
        );
        let checkout = pair.destination.join(".overgit/local/pull/tree");
        fs::write(checkout.join("f"), b"rebased resolution\n").unwrap();
        cli(&pair.destination, &["pull", "--git", "add", "f"], 0);
        cli(&pair.destination, &["pull", "--continue"], 0);
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"rebased resolution\n"
        );
        assert!(git(&pair.destination, &["status", "--porcelain"]).is_empty());
        assert!(!pair.destination.join(".overgit/local/pull").exists());
    }
    #[test]
    fn continuation_cannot_install_a_different_isolated_branch() {
        let pair = Pair::new();
        pair.diverge();
        let head = pair.head();
        let index = pair.index();
        cli(&pair.destination, &["pull", "--no-rebase"], 3);
        let checkout = pair.destination.join(".overgit/local/pull/tree");
        fs::write(checkout.join("f"), b"resolved\n").unwrap();
        cli(&pair.destination, &["pull", "--git", "add", "f"], 0);
        cli(
            &pair.destination,
            &["pull", "--git", "commit", "--no-edit"],
            0,
        );
        cli(
            &pair.destination,
            &["pull", "--git", "checkout", "-b", "other"],
            0,
        );
        cli(&pair.destination, &["pull", "--continue"], 1);
        assert_eq!(pair.head(), head);
        assert_eq!(pair.index(), index);
        assert_eq!(
            fs::read(pair.destination.join("f")).unwrap(),
            b"local overlay\n"
        );
        cli(&pair.destination, &["pull", "--abort"], 0);
    }
}
