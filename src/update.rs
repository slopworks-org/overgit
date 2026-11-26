use anyhow::{Context as _, Result, anyhow, bail, ensure};
use clap::ValueEnum;
use sha2::{Digest as _, Sha256};
use std::{
    env, fmt,
    fs::{self, File},
    io::{self, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{NamedTempFile, tempdir};

const DEFAULT_BASE_URL: &str = "https://github.com/slopworks-org/overgit/releases/download";
const ARCHIVE_MAX_BYTES: u64 = 33_554_432;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Channel {
    /// Latest builds published from the main branch
    #[default]
    Main,
}
impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Main => "main",
        })
    }
}
pub fn run(check: bool, requested: Option<Channel>, force: bool) -> Result<i32> {
    let channel = requested.map_or_else(read_channel, Ok)?;
    let base_url = release_base_url()?;
    let (next, tag) = fetch_latest(&base_url, channel)?;
    let avail = is_update_available(channel, &next, crate::VERSION);
    let cur = crate::VERSION;
    if !check {
        if force || avail {
            install_release(&base_url, &next, &tag)?;
        }
        if let Some(selected) = requested {
            write_channel(selected)?;
        }
    }
    let msg = match (check, avail, force) {
        (true, true, _) => format!("overgit {cur} -> {next} available"),
        (true, false, _) => format!("overgit {cur} is up to date"),
        (false, false, false) => format!("overgit {cur} is already up to date"),
        _ if cur == next => format!("Reinstalled overgit {next}"),
        _ => format!("Updated overgit {cur} -> {next}"),
    };
    crate::say!("{msg} (channel: {channel})");
    Ok(0)
}
fn channel_path() -> Result<PathBuf> {
    let abs = |k| {
        env::var_os(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    if let Some(dir) = abs("XDG_CONFIG_HOME") {
        return Ok(dir.join("overgit/update-channel"));
    }
    let home = abs("HOME").context("HOME is unset or relative")?;
    Ok(home.join(".config/overgit/update-channel"))
}
fn read_channel() -> Result<Channel> {
    let Ok(path) = channel_path() else {
        return Ok(Channel::default());
    };
    match fs::read_to_string(&path) {
        Ok(text) if text.trim() == "main" => Ok(Channel::Main),
        Ok(text) => {
            let (val, p) = (text.trim(), path.display());
            bail!("unsupported update channel {val:?} in {p}; run `overgit update --channel main`")
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Channel::default()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}
fn write_channel(channel: Channel) -> Result<()> {
    let path = channel_path()?;
    let parent = path.parent().context("invalid update-channel path")?;
    fs::create_dir_all(parent)?;
    let mut staged = NamedTempFile::new_in(parent)?;
    writeln!(staged, "{channel}")?;
    staged.as_file().sync_all()?;
    staged.persist(&path).map_err(|e| e.error)?;
    Ok(())
}
fn release_base_url() -> Result<String> {
    let raw = env::var("OVERGIT_UPDATE_BASE_URL").unwrap_or_default();
    let url = raw.trim().trim_end_matches('/');
    if url.is_empty() {
        return Ok(DEFAULT_BASE_URL.to_owned());
    }
    let prefixes = [
        "https://",
        "http://127.0.0.1:",
        "http://localhost:",
        "http://[::1]:",
    ];
    let msg = "OVERGIT_UPDATE_BASE_URL must use https:// (or loopback http:// for local testing)";
    ensure!(prefixes.iter().any(|p| url.starts_with(p)), "{msg}");
    Ok(url.to_owned())
}
fn run_tool(name: &str, cmd: &mut Command) -> Result<Output> {
    cmd.stdin(Stdio::null()).output().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            anyhow!("{name} is required for overgit update")
        } else {
            anyhow!("cannot run {name}: {e}")
        }
    })
}
fn fetch_file(url: &str, dest: &Path, timeout: &str, max_bytes: u64) -> Result<()> {
    let proto = ["=http,https", "=https"][usize::from(url.starts_with("https://"))];
    let max_arg = max_bytes.to_string();
    let mut cmd = Command::new("curl");
    cmd.args(["-q", "-fsSL", "--proto", proto, "--proto-redir", proto]);
    cmd.args(["--max-redirs", "5", "--max-time", timeout]);
    cmd.args(["--max-filesize", &max_arg, "-o"]);
    let output = run_tool("curl", cmd.arg(dest).args(["--", url]))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        ensure!(!detail.is_empty(), "download failed for {url}");
        bail!("download failed for {url}: {detail}");
    }
    ensure!(
        fs::metadata(dest)?.len() <= max_bytes,
        "download exceeds {max_bytes} bytes"
    );
    Ok(())
}
fn fetch_latest(base_url: &str, channel: Channel) -> Result<(String, String)> {
    let Channel::Main = channel;
    let work = tempdir()?;
    let path = work.path().join("channel.kv");
    let url = format!("{base_url}/channel-main/channel.kv");
    fetch_file(&url, &path, "15", 4096)?;
    let body = fs::read_to_string(&path)?;
    parse_main_manifest(&body).with_context(|| format!("invalid main channel manifest from {url}"))
}
fn install_release(base_url: &str, version: &str, tag: &str) -> Result<()> {
    let target = target_triple()?;
    let stem = format!("overgit-v{version}-{target}");
    let (archive_name, member) = (format!("{stem}.tar.gz"), format!("{stem}/overgit"));
    let work = tempdir()?;
    let sums_path = work.path().join("SHA256SUMS");
    let sums_url = format!("{base_url}/{tag}/SHA256SUMS");
    fetch_file(&sums_url, &sums_path, "15", 65_536)?;
    let sums = fs::read_to_string(&sums_path)?;
    let missing = format!("release {tag} does not include a build for {target}");
    let expected = find_checksum(&sums, &archive_name).context(missing)?;
    let archive = work.path().join(&archive_name);
    let url = format!("{base_url}/{tag}/{archive_name}");
    fetch_file(&url, &archive, "120", ARCHIVE_MAX_BYTES)?;
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(&archive)?, &mut hasher)?;
    let ok = format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected);
    ensure!(ok, "checksum mismatch for {archive_name}");
    let extract = work.path().join("extract");
    fs::create_dir(&extract)?;
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&archive).arg("-C");
    let output = run_tool("tar", tar.arg(&extract).arg(&member))?;
    let err = String::from_utf8_lossy(&output.stderr);
    let msg = err.trim();
    ensure!(output.status.success(), "cannot extract {member}: {msg}");
    let (dir, bin) = (extract.join(&stem), extract.join(&member));
    let is_dir = fs::symlink_metadata(&dir)?.is_dir();
    ensure!(is_dir, "extracted {stem} is not a directory");
    let meta = fs::symlink_metadata(&bin)?;
    let is_file = meta.is_file() && meta.len() > 0;
    ensure!(is_file, "extracted {member} is not a regular file");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    let ver = run_tool("overgit", Command::new(&bin).arg("--version"))?;
    let reported = String::from_utf8_lossy(&ver.stdout);
    let (want, got) = (format!("overgit {version}"), reported.trim());
    let ver_ok = ver.status.success() && got == want;
    ensure!(ver_ok, "binary reported {got:?}, expected {want:?}");
    let exe = fs::canonicalize(env::current_exe()?)?;
    let install_dir = exe.parent().context("missing executable directory")?;
    let mode = fs::metadata(&exe).map_or(0o755, |m| (m.permissions().mode() & 0o777) | 0o100);
    let mut staged = NamedTempFile::new_in(install_dir)?;
    io::copy(&mut File::open(&bin)?, staged.as_file_mut())?;
    let file = staged.as_file();
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    staged.persist(&exe).map_err(|e| e.error)?;
    Ok(())
}
fn target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        (os, arch) => bail!("unsupported platform for self-update: {os}/{arch}"),
    }
}
fn find_checksum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        let name = rest.trim_start().trim_start_matches('*');
        let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (name == file && valid).then_some(hash)
    })
}
fn strict_decimal(s: &str) -> Option<u64> {
    let valid = !s.is_empty() && (s.len() == 1 || !s.starts_with('0'));
    (valid && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}
fn parse_core(s: &str) -> Option<(u64, u64, u64)> {
    let p: Vec<_> = s.split('.').map(strict_decimal).collect::<Option<_>>()?;
    (p.len() == 3).then(|| (p[0], p[1], p[2]))
}
fn valid_sha(sha: &str) -> bool {
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    (7..=40).contains(&sha.len()) && sha.bytes().all(hex)
}
fn parse_main_version(s: &str) -> Option<((u64, u64, u64), u64, &str)> {
    let (base, suffix) = s.split_once("-main.")?;
    let (rev, sha) = suffix.split_once(".g")?;
    let rev = strict_decimal(rev).filter(|&n| n > 0)?;
    valid_sha(sha).then_some((parse_core(base)?, rev, sha))
}
fn parse_main_manifest(text: &str) -> Option<(String, String)> {
    let [mut ver, mut tag, mut commit] = [None; 3];
    for line in text.lines().filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once('=')?;
        let slot = match k {
            "version" => &mut ver,
            "tag" => &mut tag,
            "commit" => &mut commit,
            _ => continue,
        };
        if slot.replace(v).is_some() {
            return None;
        }
    }
    let (ver, tag) = (ver?, tag?);
    let (_, rev, sha) = parse_main_version(ver)?;
    let bad = commit.is_some_and(|c| !valid_sha(c) || !c.starts_with(sha));
    (tag == format!("main-{rev}-g{sha}") && !bad).then(|| (ver.to_owned(), tag.to_owned()))
}
fn is_update_available(channel: Channel, candidate: &str, current: &str) -> bool {
    let Channel::Main = channel;
    let Some((cb, cr, _)) = parse_main_version(candidate) else {
        return false;
    };
    if let Some((b, r, _)) = parse_main_version(current) {
        (cb, cr) > (b, r)
    } else {
        let core = current.split(['-', '+']).next().unwrap_or(current);
        parse_core(core).is_some_and(|b| cb >= b)
    }
}
#[cfg(test)]
#[test]
fn main_version_ordering() {
    for (c, cur, want) in [
        ("0.1.0-main.14.g1234567", "0.1.0-main.13.g3a8921d", true),
        ("0.1.0-main.13.g3a8921d", "0.1.0-main.13.g3a8921d", false),
        ("0.1.0-main.12.g3a8921d", "0.1.0-main.13.g3a8921d", false),
        ("0.2.0-main.1.g1234567", "0.1.0-main.99.g3a8921d", true),
        ("0.1.0-main.1.g3a8921d", "0.1.0", true),
        ("0.0.9-main.99.g3a8921d", "0.1.0", false),
    ] {
        assert_eq!(is_update_available(Channel::Main, c, cur), want);
    }
}
