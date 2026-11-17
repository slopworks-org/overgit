use anyhow::{Context as _, Result, bail};
use std::{
    ffi::OsStr,
    io::Write as _,
    path::Path,
    process::{Command, Output, Stdio},
};

fn scoped_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    // Repository context inherited from another Git invocation must not retarget us.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_INDEX_VERSION",
        "GIT_QUARANTINE_PATH",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
    ] {
        command.env_remove(key);
    }
    command
}

pub fn command(cwd: &Path) -> Command {
    let mut command = scoped_command(cwd);
    command
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIFF_OPTS");
    command.args([
        "-c",
        "core.quotePath=false",
        "-c",
        "status.showUntrackedFiles=no",
        "-c",
        "core.precomposeunicode=false",
    ]);
    command
}

pub fn interactive_command(cwd: &Path) -> Command {
    // User commands retain ordinary Git options, diff helpers and terminal prompts.
    scoped_command(cwd)
}

pub fn output(cmd: &mut Command, input: Option<&[u8]>) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn().context("cannot start Git")?;
    if let Some(input) = input {
        let result = child
            .stdin
            .take()
            .context("Git stdin unavailable")?
            .write_all(input);
        if let Err(error) = result {
            let output = child.wait_with_output()?;
            check(&output)?;
            return Err(error.into());
        }
    }
    Ok(child.wait_with_output()?)
}
pub fn check(output: &Output) -> Result<()> {
    if !output.status.success() {
        bail!(
            "Git exited with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(())
}
pub fn run<I, S>(cwd: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = output(command(cwd).args(args), None)?;
    check(&out)?;
    Ok(out.stdout)
}
pub fn text(data: Vec<u8>) -> Result<String> {
    let mut text = String::from_utf8(data)?;
    // Git adds one record terminator. Any preceding newline belongs to the value,
    // including repository paths and configuration values.
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}
