use std::process::Command;

#[test]
fn every_command_help_works_without_git_or_a_repository() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    for command in [
        "",
        "init",
        "clone",
        "add",
        "rm",
        "restore",
        "list",
        "which",
        "status",
        "commit",
        "diff",
        "log",
        "push",
        "fetch",
        "pull",
        "git",
        "base",
        "apply",
        "attach",
        "detach",
        "sync",
        "resolve",
        "doctor",
        "recovery",
        "recovery list",
        "recovery inspect",
        "recovery restore",
        "hooks",
        "hooks install",
        "hooks uninstall",
        "worktree",
        "worktree add",
        "worktree list",
        "worktree remove",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_overgit"))
            .current_dir(temp.path())
            .env_clear()
            .env("PATH", temp.path())
            .args(command.split_whitespace())
            .arg("--help")
            .output()?;
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage:"),
            "{command}"
        );
    }
    assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
    Ok(())
}

#[test]
fn version_matches_build_or_package_version() -> anyhow::Result<()> {
    let expected = match option_env!("OVERGIT_BUILD_VERSION") {
        Some(version) if !version.is_empty() => version,
        _ => env!("CARGO_PKG_VERSION"),
    };
    let output = Command::new(env!("CARGO_BIN_EXE_overgit"))
        .arg("--version")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("overgit {expected}")
    );
    Ok(())
}
