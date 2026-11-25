use crate::{
    git,
    model::{MANIFEST, Manifest, Snapshot, path_identity, validate_path},
};
use anyhow::{Context as _, Result, ensure};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File},
    io::Write as _,
    os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        fs::{PermissionsExt as _, symlink},
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct Repo {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub git_dir: PathBuf,
    pub common: PathBuf,
    gitlink_paths: RefCell<Option<BTreeSet<String>>>,
    base_spellings: RefCell<Option<BTreeMap<String, BTreeSet<String>>>>,
    pub(crate) inspection: Option<std::rc::Rc<RefCell<crate::inspection::ReadCache>>>,
}

impl Repo {
    pub fn discover(cwd: &Path) -> Result<Self> {
        let cwd = cwd.canonicalize()?;
        let mut probe = cwd.as_path();
        // Inside .overgit, discover the base rather than the nested overlay repository.
        for ancestor in cwd.ancestors() {
            if ancestor.file_name().is_some_and(|name| name == ".overgit") {
                probe = ancestor.parent().context("overlay has no parent")?;
                break;
            }
        }
        ensure!(
            git::text(git::run(probe, ["rev-parse", "--is-inside-work-tree"])?)? == "true",
            "not in a Git worktree"
        );
        let root = PathBuf::from(git::text(git::run(
            probe,
            ["rev-parse", "--show-toplevel"],
        )?)?)
        .canonicalize()?;
        let git_dir = PathBuf::from(git::text(git::run(
            &root,
            ["rev-parse", "--absolute-git-dir"],
        )?)?);
        let common = PathBuf::from(git::text(git::run(
            &root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?)?);
        ensure!(
            !cwd.starts_with(&git_dir),
            "refusing to run inside Git metadata"
        );
        let repo = Self {
            root,
            cwd,
            git_dir,
            common,
            gitlink_paths: RefCell::new(None),
            base_spellings: RefCell::new(None),
            inspection: None,
        };
        repo.metadata_safe()?;
        Ok(repo)
    }
    pub fn isolated(root: &Path) -> Result<Self> {
        ensure_not_symlink(root)?;
        let root = root.canonicalize()?;
        ensure!(
            root.join(".git").is_dir(),
            "isolated checkout requires its own base Git directory"
        );
        let repo = Self {
            cwd: root.clone(),
            git_dir: root.join(".git"),
            common: root.join(".git"),
            root,
            gitlink_paths: RefCell::new(None),
            base_spellings: RefCell::new(None),
            inspection: None,
        };
        repo.metadata_safe()?;
        repo.require_overlay()?;
        Ok(repo)
    }
    pub fn metadata_safe(&self) -> Result<()> {
        for relative in [
            ".git",
            ".overgit",
            ".overgit/.git",
            ".overgit/local",
            MANIFEST,
        ] {
            ensure_not_symlink(&self.root.join(relative))?;
        }
        ensure_git_metadata(&self.common)?;
        if self.git_dir != self.common {
            ensure_git_metadata(&self.git_dir)?;
        }
        let base_gitfile = self.root.join(".git");
        if base_gitfile.is_file() {
            ensure_gitfile_metadata(&base_gitfile)?;
        }
        ensure_gitfile_metadata(&self.overlay_dir())?;
        let hooks = git::output(
            self.base_cmd()
                .args(["config", "--path", "--get", "core.hooksPath"]),
            None,
        )?;
        ensure!(
            hooks.status.success() || hooks.status.code() == Some(1),
            "cannot read base hooks path"
        );
        if hooks.status.success() {
            ensure_not_symlink(&self.root.join(git::text(hooks.stdout)?))?;
        }
        ensure!(
            !self.cwd.starts_with(self.root.join(".overgit/.git")),
            "refusing to run inside overlay Git metadata"
        );
        Ok(())
    }
    pub fn overlay_dir(&self) -> PathBuf {
        self.root.join(".overgit/.git")
    }
    pub fn local(&self, name: &str) -> PathBuf {
        self.root.join(".overgit/local").join(name)
    }
    pub fn detached(&self) -> bool {
        self.local("detached.json").exists()
    }
    pub fn require_overlay(&self) -> Result<()> {
        ensure!(
            self.overlay_dir().exists(),
            "no overlay; run `overgit init` or `overgit clone`"
        );
        self.overlay(["rev-parse", "--git-dir"])?;
        Ok(())
    }
    pub fn base_cmd(&self) -> Command {
        git::command(&self.root)
    }
    pub fn overlay_cmd(&self) -> Command {
        let mut cmd = git::command(&self.root);
        cmd.env("GIT_DIR", self.overlay_dir())
            .env("GIT_WORK_TREE", &self.root);
        cmd
    }
    pub fn interactive_cmd(&self) -> Command {
        let mut command = git::interactive_command(&self.cwd);
        command
            .env("GIT_DIR", self.overlay_dir())
            .env("GIT_WORK_TREE", &self.root);
        command
    }
    pub fn base<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        git::run(&self.root, args)
    }
    pub fn overlay<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = git::output(self.overlay_cmd().args(args), None)?;
        git::check(&output)?;
        Ok(output.stdout)
    }
    pub fn passthrough(&self, args: &[String]) -> Result<i32> {
        let status = self
            .interactive_cmd()
            .current_dir(&self.cwd)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        Ok(status.code().unwrap_or(1))
    }
    pub fn lock(&self) -> Result<File> {
        self.metadata_safe()?;
        ensure_not_symlink(&self.common.join("overgit.lock"))?;
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.common.join("overgit.lock"))?;
        file.try_lock()
            .context("another overgit command holds the repository lock")?;
        Ok(file)
    }
    pub fn assert_idle(&self) -> Result<()> {
        for marker in [
            "MERGE_HEAD",
            "rebase-merge",
            "rebase-apply",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
            "sequencer",
        ] {
            ensure!(
                !self.git_dir.join(marker).exists(),
                "base operation in progress ({marker}); finish or abort it first"
            );
        }
        let sparse = git::output(
            self.base_cmd()
                .args(["config", "--bool", "core.sparseCheckout"]),
            None,
        )?;
        ensure!(
            sparse.status.code() == Some(1) || sparse.status.success(),
            "cannot read sparse checkout configuration"
        );
        ensure!(
            sparse.stdout != b"true\n",
            "sparse checkout is unsupported; disable it first"
        );
        Ok(())
    }
    pub fn assert_attached(&self) -> Result<()> {
        ensure!(
            !self.detached(),
            "overlay is detached; run `overgit attach` first"
        );
        Ok(())
    }
    pub fn assert_no_sync(&self) -> Result<()> {
        ensure!(
            !self.local("sync.json").exists(),
            "sync in progress; resolve and continue, or abort it first"
        );
        Ok(())
    }
    pub fn manifest(&self) -> Result<Manifest> {
        let path = self.root.join(MANIFEST);
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(&path)
                .with_context(|| format!("cannot read {}; run doctor --fix", path.display()))?,
        )?;
        manifest.validate()?;
        Ok(manifest)
    }
    pub fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        manifest.validate()?;
        let mut bytes = serde_json::to_vec_pretty(manifest)?;
        bytes.push(b'\n');
        let snapshot = Snapshot {
            mode: "100644".into(),
            data: bytes,
        };
        crate::ops::rescue_index_before_replacing(self, MANIFEST, Some(&snapshot))?;
        atomic_write(&self.root.join(MANIFEST), &snapshot.data)?;
        self.stage(MANIFEST, &snapshot)
    }
    pub fn relative_path(&self, input: &Path) -> Result<String> {
        let absolute = if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.cwd.join(input)
        };
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            if normalized.starts_with(&self.root)
                && normalized != self.root
                && let Ok(meta) = fs::symlink_metadata(&normalized)
            {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "symlink parent {}",
                    normalized.display()
                );
            }
            match component {
                Component::ParentDir => {
                    ensure!(normalized.pop(), "path escapes root");
                }
                Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }
        let relative = normalized
            .strip_prefix(&self.root)
            .context("path is outside the base repository")?;
        Ok(relative
            .to_str()
            .context("non-UTF-8 paths are unsupported")?
            .to_owned())
    }
    pub fn path(&self, input: &Path) -> Result<String> {
        let relative = self.relative_path(input)?;
        validate_path(&relative)?;
        self.safe_path(&relative)?;
        Ok(relative)
    }
    pub fn safe_path(&self, path: &str) -> Result<PathBuf> {
        validate_path(path)?;
        self.assert_base_spelling(path)?;
        let absolute = self.root.join(path);
        let mut parent = absolute.parent();
        while let Some(dir) = parent {
            if dir == self.root {
                break;
            }
            if let Ok(meta) = fs::symlink_metadata(dir) {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "unsafe parent {}",
                    dir.display()
                );
            }
            ensure!(
                !dir.join(".git").exists(),
                "nested repositories and submodules are unsupported at {}",
                dir.display()
            );
            parent = dir.parent();
        }
        // Unpopulated gitlinks still need checking. Decode only gitlink names, not
        // unrelated filenames in the base's raw index, and read this once per command.
        for tracked in self.gitlinks()? {
            ensure!(
                !Path::new(&path_identity(path)).starts_with(path_identity(&tracked)),
                "submodules cannot be overlaid at {path:?}"
            );
        }
        Ok(absolute)
    }
    fn assert_base_spelling(&self, path: &str) -> Result<()> {
        if self.base_spellings.borrow().is_none() {
            let mut spellings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for tracked in self.index(false)?.into_keys() {
                spellings
                    .entry(path_identity(&tracked))
                    .or_default()
                    .insert(tracked);
            }
            *self.base_spellings.borrow_mut() = Some(spellings);
        }
        if let Some(spellings) = self.base_spellings.borrow().as_ref()
            && let Some(aliases) = spellings.get(&path_identity(path))
        {
            ensure!(
                aliases.iter().all(|alias| alias == path),
                "ambiguous path {path:?}; use the exact base index spelling {aliases:?}"
            );
        }
        Ok(())
    }
    pub fn gitlinks(&self) -> Result<BTreeSet<String>> {
        if let Some(paths) = self.gitlink_paths.borrow().as_ref() {
            return Ok(paths.clone());
        }
        let bytes = self.base(["ls-files", "--stage", "-z"])?;
        let mut paths = BTreeSet::new();
        for record in bytes
            .split(|byte| *byte == 0)
            .filter(|record| record.starts_with(b"160000 "))
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git index record")?;
            if let Ok(path) = std::str::from_utf8(&record[tab + 1..]) {
                paths.insert(path.to_owned());
            }
        }
        *self.gitlink_paths.borrow_mut() = Some(paths.clone());
        Ok(paths)
    }
    pub fn index(&self, overlay: bool) -> Result<BTreeMap<String, (String, String)>> {
        let bytes = self.read_index(overlay, None)?;
        let mut result = BTreeMap::new();
        for record in bytes
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git index record")?;
            let header = std::str::from_utf8(&record[..tab])?;
            let mut fields = header.split_whitespace();
            let mode = fields.next().context("index mode missing")?.to_owned();
            let oid = fields.next().context("index oid missing")?.to_owned();
            let stage = fields.next().context("index stage missing")?;
            if stage != "0" {
                ensure!(
                    !overlay,
                    "unmerged overlay index; finish the Git operation first"
                );
                continue;
            }
            let path = match String::from_utf8(record[tab + 1..].to_vec()) {
                Ok(path) => path,
                Err(error) if overlay => return Err(error.into()),
                Err(_) => continue,
            };
            result.insert(path, (mode, oid));
        }
        Ok(result)
    }
    pub fn index_file(&self, path: &str, overlay: bool) -> Result<Option<(String, String)>> {
        let bytes = self.read_index(overlay, Some(path))?;
        for record in bytes
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git index record")?;
            if &record[tab + 1..] != path.as_bytes() {
                continue;
            }
            let mut fields = std::str::from_utf8(&record[..tab])?.split_whitespace();
            let mode = fields.next().context("index mode missing")?.to_owned();
            let oid = fields.next().context("index oid missing")?.to_owned();
            ensure!(
                fields.next() == Some("0"),
                "unmerged index at {path:?}; finish the Git operation first"
            );
            return Ok(Some((mode, oid)));
        }
        Ok(None)
    }
    pub fn indexed(&self, path: &str, overlay: bool) -> Result<Option<Snapshot>> {
        let Some((mode, oid)) = self.index_file(path, overlay)? else {
            return Ok(None);
        };
        ensure!(mode != "160000", "submodules are unsupported at {path:?}");
        let data = self.read_object(path, overlay, &mode, &oid)?;
        Ok(Some(Snapshot { mode, data }))
    }
    pub fn committed(&self, path: &str) -> Result<Option<Snapshot>> {
        let head = git::output(
            self.overlay_cmd()
                .args(["rev-parse", "--verify", "--quiet", "HEAD"]),
            None,
        )?;
        if !head.status.success() {
            ensure!(head.status.code() == Some(1), "cannot read overlay HEAD");
            return Ok(None);
        }
        let bytes = self.overlay(["ls-tree", "-z", "HEAD", "--", path])?;
        let Some(record) = bytes
            .split(|byte| *byte == 0)
            .find(|record| !record.is_empty())
        else {
            return Ok(None);
        };
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("invalid Git tree record")?;
        let mut fields = std::str::from_utf8(&record[..tab])?.split_whitespace();
        let mode = fields.next().context("missing tree mode")?.to_owned();
        ensure!(fields.next() == Some("blob"), "not a blob at {path:?}");
        let oid = fields.next().context("missing tree oid")?;
        let snapshot = Snapshot {
            mode,
            data: self.overlay(["cat-file", "blob", oid])?,
        };
        snapshot.validate()?;
        Ok(Some(snapshot))
    }
    pub fn work(&self, path: &str) -> Result<Option<Snapshot>> {
        let absolute = self.safe_path(path)?;
        read_snapshot(&absolute)
    }
    pub fn validate_destination_components(&self, path: &str) -> Result<()> {
        self.safe_path(path)?;
        let mut prefix = self.root.clone();
        let mut existing_parent = self.root.clone();
        let mut missing = false;
        for component in Path::new(path).components() {
            prefix.push(component.as_os_str());
            match fs::symlink_metadata(&prefix) {
                Ok(metadata) if !missing && metadata.is_dir() => {
                    existing_parent.clone_from(&prefix);
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
                Err(error) => return Err(error.into()),
            }
            if missing {
                // A missing ancestor can mask an invalid later filename. Probe the
                // same component on its filesystem without creating any directories.
                match fs::symlink_metadata(existing_parent.join(component.as_os_str())) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
    pub fn stage(&self, path: &str, value: &Snapshot) -> Result<()> {
        value.validate()?;
        let output = git::output(
            self.overlay_cmd().args(["hash-object", "-w", "--stdin"]),
            Some(&value.data),
        )?;
        git::check(&output)?;
        let oid = git::text(output.stdout)?;
        self.overlay([
            "update-index",
            "--add",
            "--cacheinfo",
            &value.mode,
            &oid,
            path,
        ])?;
        Ok(())
    }
    pub fn unstage_path(&self, path: &str) -> Result<()> {
        self.overlay(["update-index", "--force-remove", "--", path])?;
        Ok(())
    }
    pub fn set_base_flags<'a>(
        &self,
        updates: impl IntoIterator<Item = (&'a str, bool, bool)>,
    ) -> Result<()> {
        let wanted: BTreeMap<_, _> = updates.into_iter().map(|(p, s, a)| (p, (s, a))).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let flags = self.base(["ls-files", "-v", "-z"])?;
        let mut buckets: [Vec<u8>; 4] = Default::default();
        for row in flags.split(|byte| *byte == 0).filter(|row| !row.is_empty()) {
            let Some(path) = row.get(2..).and_then(|b| std::str::from_utf8(b).ok()) else {
                continue;
            };
            let Some(&(want_skip, want_assume)) = wanted.get(path) else {
                continue;
            };
            if row[0].is_ascii_lowercase() != want_assume {
                let buf = &mut buckets[usize::from(want_assume)];
                buf.extend_from_slice(path.as_bytes());
                buf.push(0);
            }
            if row[0].eq_ignore_ascii_case(&b'S') != want_skip {
                let buf = &mut buckets[2 + usize::from(!want_skip)];
                buf.extend_from_slice(path.as_bytes());
                buf.push(0);
            }
        }
        let options = [
            "--no-assume-unchanged",
            "--assume-unchanged",
            "--skip-worktree",
            "--no-skip-worktree",
        ];
        for (option, input) in options.into_iter().zip(buckets) {
            if !input.is_empty() {
                git::check(&git::output(
                    self.base_cmd()
                        .args(["update-index", option, "-z", "--stdin"]),
                    Some(&input),
                )?)?;
            }
        }
        Ok(())
    }
    pub fn skip_many<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
        hide: bool,
    ) -> Result<()> {
        self.set_base_flags(paths.into_iter().map(|path| (path, hide, false)))
    }
    pub fn skip(&self, path: &str, hide: bool) -> Result<()> {
        self.skip_many([path], hide)
    }
    pub fn staged_base_safe(&self, path: &str) -> Result<()> {
        let bytes = self.base(["diff", "--cached", "--name-only", "-z", "--", path])?;
        ensure!(
            bytes.is_empty(),
            "base has staged changes at {path:?}; commit or unstage them first"
        );
        let flags = self.base(["ls-files", "-v", "-z", "--", path])?;
        let owned = self
            .manifest()
            .is_ok_and(|manifest| manifest.entries.contains_key(path));
        if let Some(flag) = flags.first() {
            ensure!(
                owned || (*flag != b'S' && !flag.is_ascii_lowercase()),
                "base index flags at {path:?} are unowned; clear them explicitly first"
            );
        }
        Ok(())
    }
    pub fn replace(
        &self,
        path: &str,
        value: Option<&Snapshot>,
        backup: bool,
        force: bool,
    ) -> Result<()> {
        let absolute = self.safe_path(path)?;
        if let Ok(meta) = fs::symlink_metadata(&absolute) {
            if meta.is_dir() || (!meta.is_file() && !meta.file_type().is_symlink()) {
                ensure!(
                    force,
                    "non-file obstruction at {path:?}; use --force to rescue and replace it"
                );
                let directory = self.backup_dir()?;
                let destination = directory.join("content");
                atomic_write(&directory.join("path"), path.as_bytes())?;
                recovery_metadata(
                    &directory,
                    path,
                    "replaced directory obstruction",
                    if meta.is_dir() {
                        "directory"
                    } else {
                        "special"
                    },
                    &format!("{:06o}", meta.permissions().mode()),
                )?;
                fs::rename(&absolute, &destination)?;
                eprintln!("rescued {path:?} to {}", destination.display());
            } else if backup {
                self.backup(path)?;
            }
        }
        match value {
            Some(value) => write_snapshot(&absolute, value)?,
            None => match fs::remove_file(&absolute) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
        Ok(())
    }
    fn backup_dir(&self) -> Result<PathBuf> {
        let parent = self.local("backups");
        ensure_not_symlink(&parent)?;
        fs::create_dir_all(&parent)?;
        Ok(tempfile::Builder::new()
            .prefix("rescue-")
            .tempdir_in(parent)?
            .keep())
    }
    pub fn backup(&self, path: &str) -> Result<()> {
        if let Some(value) = self.work(path)? {
            self.rescue(path, &value)?;
        }
        Ok(())
    }
    pub fn rescue(&self, path: &str, value: &Snapshot) -> Result<()> {
        let directory = self.backup_dir()?;
        atomic_write(&directory.join("path"), path.as_bytes())?;
        // Store links as bytes, never as a live symlink into a user's tree.
        atomic_write(
            &directory.join("snapshot.json"),
            &serde_json::to_vec(value)?,
        )?;
        atomic_write(&directory.join("content"), &value.data)?;
        recovery_metadata(
            &directory,
            path,
            "replaced local content",
            if value.mode == "120000" {
                "symlink"
            } else {
                "file"
            },
            &value.mode,
        )?;
        eprintln!("backed up {path:?} to {}", directory.display());
        Ok(())
    }
}

fn recovery_metadata(
    directory: &Path,
    original_path: &str,
    reason: &str,
    original_type: &str,
    original_mode: &str,
) -> Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs();
    let recovery = serde_json::json!({
        "timestamp_unix": timestamp,
        "original_path": original_path,
        "reason": reason,
        "original_type": original_type,
        "original_mode": original_mode,
        "content_location": "content",
    });
    atomic_write(
        &directory.join("recovery.json"),
        &serde_json::to_vec_pretty(&recovery)?,
    )
}

pub fn ensure_not_symlink(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut checked = PathBuf::new();
    for component in absolute.components() {
        checked.push(component.as_os_str());
        match fs::symlink_metadata(&checked) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink() || trusted_platform_alias(&checked),
                "refusing symlink at {}",
                checked.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn trusted_platform_alias(path: &Path) -> bool {
    cfg!(target_os = "macos") && matches!(path.to_str(), Some("/tmp" | "/var" | "/etc"))
}
fn ensure_gitfile_metadata(path: &Path) -> Result<()> {
    if path.is_dir() {
        return ensure_git_metadata(path);
    }
    if path.is_file()
        && let Ok(text) = fs::read_to_string(path)
        && let Some(target) = text.trim().strip_prefix("gitdir: ")
    {
        let directory = path.parent().context("gitfile has no parent")?.join(target);
        ensure_git_metadata(&directory)?;
        let common_file = directory.join("commondir");
        ensure_not_symlink(&common_file)?;
        if let Ok(common) = fs::read_to_string(common_file) {
            ensure_git_metadata(&directory.join(common.trim()))?;
        }
    }
    Ok(())
}
fn ensure_git_metadata(directory: &Path) -> Result<()> {
    ensure_not_symlink(directory)?;
    for relative in [
        "config",
        "config.worktree",
        "commondir",
        "gitdir",
        "index",
        "index.lock",
        "HEAD",
        "packed-refs",
        "overgit.lock",
        "info",
        "info/exclude",
        "hooks",
        "objects",
        "objects/info",
        "objects/info/alternates",
        "objects/pack",
        "refs",
        "logs",
    ] {
        ensure_not_symlink(&directory.join(relative))?;
    }
    for relative in ["refs", "logs"] {
        ensure_metadata_tree(&directory.join(relative))?;
    }
    let objects = directory.join("objects");
    if objects.is_dir() {
        for entry in fs::read_dir(objects)? {
            ensure_not_symlink(&entry?.path())?;
        }
    }
    Ok(())
}
fn ensure_metadata_tree(directory: &Path) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        ensure_not_symlink(&entry.path())?;
        if entry.file_type()?.is_dir() {
            ensure_metadata_tree(&entry.path())?;
        }
    }
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_not_symlink(path)?;
    let parent = path.parent().context("file has no parent")?;
    fs::create_dir_all(parent)?;
    ensure_not_symlink(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn read_snapshot(path: &Path) -> Result<Option<Snapshot>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if meta.file_type().is_symlink() {
        return Ok(Some(Snapshot {
            mode: "120000".into(),
            data: fs::read_link(path)?.as_os_str().as_bytes().to_vec(),
        }));
    }
    ensure!(
        meta.is_file(),
        "not a regular file or symlink at {}",
        path.display()
    );
    Ok(Some(Snapshot {
        mode: if meta.permissions().mode() & 0o111 != 0 {
            "100755"
        } else {
            "100644"
        }
        .into(),
        data: fs::read(path)?,
    }))
}
pub fn write_snapshot(path: &Path, value: &Snapshot) -> Result<()> {
    value.validate()?;
    let parent = path.parent().context("file has no parent")?;
    ensure_not_symlink(parent)?;
    fs::create_dir_all(parent)?;
    ensure_not_symlink(parent)?;
    if value.mode == "120000" {
        let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
        fs::remove_file(&temporary)?;
        symlink(std::ffi::OsString::from_vec(value.data.clone()), &temporary)?;
        fs::rename(&temporary, path)?;
    } else {
        // Atomic rename replaces a final-component symlink without following it.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&value.data)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(if value.mode == "100755" {
                0o755
            } else {
                0o644
            }))?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
    }
    Ok(())
}
