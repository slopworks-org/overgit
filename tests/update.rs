use anyhow::{Result, bail};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read as _, Write as _},
    net::TcpListener,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;

struct TestServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(routes: HashMap<String, Vec<u8>>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        drop(stream.set_read_timeout(Some(Duration::from_secs(5))));
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("");
                        if let Some(body) = routes.get(path) {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            drop(stream.write_all(header.as_bytes()));
                            drop(stream.write_all(body));
                        } else {
                            drop(stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            ));
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{addr}"),
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

fn target_triple() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        other => bail!("unsupported test platform: {other:?}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn build_archive(version: &str, reported_version: &str) -> Result<(String, Vec<u8>)> {
    let work = TempDir::new()?;
    let target = target_triple()?;
    let stem = format!("overgit-v{version}-{target}");
    let dir = work.path().join(&stem);
    fs::create_dir(&dir)?;
    let script = format!("#!/bin/sh\necho \"overgit {reported_version}\"\n");
    let bin = dir.join("overgit");
    fs::write(&bin, script)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    let archive_name = format!("{stem}.tar.gz");
    let archive_path = work.path().join(&archive_name);
    let status = Command::new("tar")
        .args(["-C"])
        .arg(work.path())
        .args(["-czf"])
        .arg(&archive_path)
        .arg(&stem)
        .status()?;
    assert!(status.success());
    Ok((archive_name, fs::read(&archive_path)?))
}

fn release_routes(
    version: &str,
    tag: &str,
    commit: &str,
    reported_version: &str,
) -> Result<HashMap<String, Vec<u8>>> {
    let (archive_name, archive_bytes) = build_archive(version, reported_version)?;
    let sum = sha256_hex(&archive_bytes);
    let mut routes = HashMap::new();
    routes.insert(
        "/channel-main/channel.kv".into(),
        format!("version={version}\ntag={tag}\ncommit={commit}\n").into_bytes(),
    );
    routes.insert(
        format!("/{tag}/SHA256SUMS"),
        format!("{sum}  {archive_name}\n").into_bytes(),
    );
    routes.insert(format!("/{tag}/{archive_name}"), archive_bytes);
    Ok(routes)
}

fn copy_overgit(dir: &Path) -> Result<PathBuf> {
    let dst = dir.join("overgit");
    fs::copy(env!("CARGO_BIN_EXE_overgit"), &dst)?;
    fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
    Ok(dst)
}

fn run_update(bin: &Path, config: &Path, base_url: &str, args: &[&str]) -> Result<Output> {
    let cwd = config.parent().unwrap_or(config);
    Ok(Command::new(bin)
        .current_dir(cwd)
        .args(["update"])
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .env("OVERGIT_UPDATE_BASE_URL", base_url)
        .output()?)
}

#[test]
fn update_check_reports_available_without_modifying_binary_or_config() -> Result<()> {
    let temp = TempDir::new()?;
    let bin = copy_overgit(temp.path())?;
    let config = temp.path().join("config");
    let before = fs::read(&bin)?;

    let routes = release_routes(
        "0.1.0-main.99.g1234567",
        "main-99-g1234567",
        "1234567890abcdef1234567890abcdef12345678",
        "0.1.0-main.99.g1234567",
    )?;
    let server = TestServer::start(routes)?;

    let out = run_update(
        &bin,
        &config,
        &server.base_url,
        &["--check", "--channel", "main"],
    )?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("0.1.0-main.99.g1234567 available (channel: main)"),
        "{stdout}"
    );
    assert_eq!(fs::read(&bin)?, before);
    assert!(!config.join("overgit/update-channel").exists());

    let quiet = Command::new(&bin)
        .current_dir(temp.path())
        .args(["-q", "update", "--check"])
        .env("XDG_CONFIG_HOME", &config)
        .env("OVERGIT_UPDATE_BASE_URL", &server.base_url)
        .output()?;
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());
    Ok(())
}

#[test]
fn update_installs_verified_binary_and_persists_channel() -> Result<()> {
    let temp = TempDir::new()?;
    let bin_dir = temp.path().join("bin");
    fs::create_dir(&bin_dir)?;
    let bin = copy_overgit(&bin_dir)?;
    let config = temp.path().join("config");

    let routes = release_routes(
        "0.1.0-main.99.g1234567",
        "main-99-g1234567",
        "1234567890abcdef1234567890abcdef12345678",
        "0.1.0-main.99.g1234567",
    )?;
    let server = TestServer::start(routes)?;

    let out = run_update(&bin, &config, &server.base_url, &["--channel", "main"])?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Updated overgit"), "{stdout}");
    assert!(
        stdout.contains("0.1.0-main.99.g1234567 (channel: main)"),
        "{stdout}"
    );

    assert_eq!(
        fs::read_to_string(config.join("overgit/update-channel"))?,
        "main\n"
    );
    assert_eq!(fs::read_dir(&bin_dir)?.count(), 1);

    let version = Command::new(&bin).arg("--version").output()?;
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "overgit 0.1.0-main.99.g1234567"
    );
    Ok(())
}

#[test]
fn update_follows_symlink_and_replaces_target_atomically() -> Result<()> {
    let temp = TempDir::new()?;
    let real_dir = temp.path().join("real");
    let link_dir = temp.path().join("link");
    fs::create_dir(&real_dir)?;
    fs::create_dir(&link_dir)?;
    let real_bin = copy_overgit(&real_dir)?;
    let link_bin = link_dir.join("overgit");
    symlink(&real_bin, &link_bin)?;
    let config = temp.path().join("config");

    let routes = release_routes(
        "0.1.0-main.42.gabcdef0",
        "main-42-gabcdef0",
        "abcdef0123456789abcdef0123456789abcdef01",
        "0.1.0-main.42.gabcdef0",
    )?;
    let server = TestServer::start(routes)?;

    let out = run_update(&link_bin, &config, &server.base_url, &[])?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::symlink_metadata(&link_bin)?.file_type().is_symlink());
    assert_eq!(fs::read_link(&link_bin)?, real_bin);

    let version = Command::new(&link_bin).arg("--version").output()?;
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "overgit 0.1.0-main.42.gabcdef0"
    );
    Ok(())
}

#[test]
fn update_skips_when_up_to_date_unless_forced() -> Result<()> {
    let temp = TempDir::new()?;
    let bin = copy_overgit(temp.path())?;
    let config = temp.path().join("config");
    let before = fs::read(&bin)?;

    let routes = release_routes(
        "0.0.1-main.1.gabcdef0",
        "main-1-gabcdef0",
        "abcdef0123456789abcdef0123456789abcdef01",
        "0.0.1-main.1.gabcdef0",
    )?;
    let server = TestServer::start(routes)?;

    let out = run_update(&bin, &config, &server.base_url, &[])?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("already up to date (channel: main)"));
    assert_eq!(fs::read(&bin)?, before);

    let forced = run_update(&bin, &config, &server.base_url, &["--force"])?;
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let version = Command::new(&bin).arg("--version").output()?;
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "overgit 0.0.1-main.1.gabcdef0"
    );
    Ok(())
}

#[test]
fn update_rejects_bad_checksum_version_manifest_and_channel() -> Result<()> {
    let temp = TempDir::new()?;
    let bin = copy_overgit(temp.path())?;
    let config = temp.path().join("config");
    let before = fs::read(&bin)?;

    // 1. Checksum mismatch
    let mut bad_sum = release_routes(
        "0.1.0-main.99.g1234567",
        "main-99-g1234567",
        "1234567890abcdef1234567890abcdef12345678",
        "0.1.0-main.99.g1234567",
    )?;
    let archive_name = format!(
        "overgit-v0.1.0-main.99.g1234567-{}.tar.gz",
        target_triple()?
    );
    bad_sum.insert(
        "/main-99-g1234567/SHA256SUMS".into(),
        format!("{}  {archive_name}\n", "0".repeat(64)).into_bytes(),
    );
    let server = TestServer::start(bad_sum)?;
    let out = run_update(&bin, &config, &server.base_url, &["--channel", "main"])?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert_eq!(fs::read(&bin)?, before);
    assert!(!config.join("overgit/update-channel").exists());
    drop(server);

    // 2. Missing target in SHA256SUMS
    let mut missing_target = HashMap::new();
    missing_target.insert(
        "/channel-main/channel.kv".into(),
        b"version=0.1.0-main.99.g1234567\ntag=main-99-g1234567\n".to_vec(),
    );
    missing_target.insert(
        "/main-99-g1234567/SHA256SUMS".into(),
        format!(
            "{}  overgit-v0.1.0-main.99.g1234567-other.tar.gz\n",
            "a".repeat(64)
        )
        .into_bytes(),
    );
    let server = TestServer::start(missing_target)?;
    let out = run_update(&bin, &config, &server.base_url, &[])?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not include a build for"));
    drop(server);

    // 3. Wrong --version reported by downloaded binary
    let bad_ver = release_routes(
        "0.1.0-main.99.g1234567",
        "main-99-g1234567",
        "1234567890abcdef1234567890abcdef12345678",
        "0.1.0-main.1.g0000000",
    )?;
    let server = TestServer::start(bad_ver)?;
    let out = run_update(&bin, &config, &server.base_url, &[])?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected"));
    assert_eq!(fs::read(&bin)?, before);
    Ok(())
}

#[test]
fn update_rejects_invalid_manifest_and_recovers_corrupted_channel() -> Result<()> {
    let temp = TempDir::new()?;
    let bin = copy_overgit(temp.path())?;
    let config = temp.path().join("config");

    for manifest in [
        "version=0.1.0-main.99.g1234567\ntag=../evil\n",
        "version=0.1.0-main.99.g1234567\ntag=main-99-g1234567\ntag=main-99-g1234567\n",
        "version=0.1.0-main.99.g1234567\ntag=main-99-g1234567\ncommit=0000000\n",
        "version=01.0.0-main.1.g1234567\ntag=main-1-g1234567\n",
    ] {
        let mut bad_manifest = HashMap::new();
        bad_manifest.insert(
            "/channel-main/channel.kv".into(),
            manifest.as_bytes().to_vec(),
        );
        let server = TestServer::start(bad_manifest)?;
        let out = run_update(&bin, &config, &server.base_url, &["--check"])?;
        assert!(!out.status.success(), "{manifest}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("invalid main channel manifest"));
    }

    let out = Command::new(&bin)
        .args(["update", "--channel", "nightly"])
        .output()?;
    assert_eq!(out.status.code(), Some(2));
    let out = Command::new(&bin)
        .args(["update", "--check", "--force"])
        .output()?;
    assert_eq!(out.status.code(), Some(2));

    fs::create_dir_all(config.join("overgit"))?;
    fs::write(config.join("overgit/update-channel"), "bogus\n")?;
    let routes = release_routes(
        "0.0.1-main.1.gabcdef0",
        "main-1-gabcdef0",
        "abcdef0123456789abcdef0123456789abcdef01",
        "0.0.1-main.1.gabcdef0",
    )?;
    let server = TestServer::start(routes)?;
    let out = run_update(&bin, &config, &server.base_url, &["--check"])?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported update channel"));

    let out = run_update(&bin, &config, &server.base_url, &["--channel", "main"])?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(config.join("overgit/update-channel"))?,
        "main\n"
    );
    Ok(())
}

#[test]
fn update_rejects_non_https_remote_base_url() -> Result<()> {
    let temp = TempDir::new()?;
    let config = temp.path().join("config");
    let out = run_update(
        Path::new(env!("CARGO_BIN_EXE_overgit")),
        &config,
        "http://example.com/releases",
        &["--check"],
    )?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("must use https://"));
    Ok(())
}

#[test]
fn update_falls_back_to_home_when_xdg_is_unset_or_relative() -> Result<()> {
    let temp = TempDir::new()?;
    let bin = copy_overgit(temp.path())?;
    let home = temp.path().join("home");
    fs::create_dir(&home)?;

    let routes = release_routes(
        "0.0.1-main.1.gabcdef0",
        "main-1-gabcdef0",
        "abcdef0123456789abcdef0123456789abcdef01",
        "0.0.1-main.1.gabcdef0",
    )?;
    let server = TestServer::start(routes)?;

    let out = Command::new(&bin)
        .current_dir(temp.path())
        .args(["update", "--channel", "main"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", "relative/path")
        .env("OVERGIT_UPDATE_BASE_URL", &server.base_url)
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(home.join(".config/overgit/update-channel"))?,
        "main\n"
    );
    Ok(())
}

#[test]
fn update_rejects_symlink_binary_inside_archive() -> Result<()> {
    let temp = TempDir::new()?;
    let bin = copy_overgit(temp.path())?;
    let config = temp.path().join("config");
    let before = fs::read(&bin)?;

    let work = TempDir::new()?;
    let target = target_triple()?;
    let version = "0.1.0-main.99.g1234567";
    let tag = "main-99-g1234567";
    let stem = format!("overgit-v{version}-{target}");
    let dir = work.path().join(&stem);
    fs::create_dir(&dir)?;
    symlink("/bin/sh", dir.join("overgit"))?;
    let archive_name = format!("{stem}.tar.gz");
    let archive_path = work.path().join(&archive_name);
    let status = Command::new("tar")
        .args(["-C"])
        .arg(work.path())
        .args(["-czf"])
        .arg(&archive_path)
        .arg(&stem)
        .status()?;
    assert!(status.success());
    let bytes = fs::read(&archive_path)?;
    let sum = sha256_hex(&bytes);

    let mut routes = HashMap::new();
    routes.insert(
        "/channel-main/channel.kv".into(),
        format!("version={version}\ntag={tag}\n").into_bytes(),
    );
    routes.insert(
        format!("/{tag}/SHA256SUMS"),
        format!("{sum}  {archive_name}\n").into_bytes(),
    );
    routes.insert(format!("/{tag}/{archive_name}"), bytes);
    let server = TestServer::start(routes)?;

    let out = run_update(&bin, &config, &server.base_url, &[])?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a regular file"));
    assert_eq!(fs::read(&bin)?, before);
    Ok(())
}
