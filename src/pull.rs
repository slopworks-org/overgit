use crate::{
    git,
    model::{Kind, MANIFEST, Manifest, Snapshot},
    ops,
    repo::{Repo, atomic_write},
};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::Path,
    sync::atomic::Ordering,
};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    version: u32,
    started_at_unix: u64,
    head: String,
    branch: Option<String>,
    refs: BTreeMap<String, Reference>,
    manifest: Vec<u8>,
    index: Vec<u8>,
    base_head: Option<String>,
    base_index: Option<Vec<u8>>,
    work: BTreeMap<String, Option<Snapshot>>,
}
impl Session {
    fn capture(repo: &Repo, manifest: &Manifest) -> Result<Self> {
        let files = repo.managed_files(manifest)?;
        let repo = &repo.with_layout(files.keys());
        Ok(Self {
            version: 1,
            started_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
            head: git::text(repo.overlay(["rev-parse", "HEAD"])?)?,
            branch: head_branch(repo)?,
            refs: refs(repo)?,
            manifest: fs::read(repo.root.join(MANIFEST))?,
            index: fs::read(index_path(repo, true)?)?,
            base_head: base_head(repo)?,
            base_index: optional_bytes(&index_path(repo, false)?)?,
            work: files
                .into_keys()
                .map(|path| Ok((path.clone(), repo.work(&path)?)))
                .collect::<Result<_>>()?,
        })
    }
    fn ensure_unchanged(&self, repo: &Repo) -> Result<()> {
        let repo = &repo.with_layout(self.work.keys());
        ensure!(self.version == 1, "unsupported pending pull version");
        ensure!(
            self.head == git::text(repo.overlay(["rev-parse", "HEAD"])?)?
                && self.branch == head_branch(repo)?
                && self.refs == refs(repo)?
                && self.manifest == fs::read(repo.root.join(MANIFEST))?
                && self.index == fs::read(index_path(repo, true)?)?
                && self.base_head == base_head(repo)?
                && self.base_index == optional_bytes(&index_path(repo, false)?)?,
            "original repository changed while pull was pending; preserve those changes, then abort this pull and retry"
        );
        for (path, value) in &self.work {
            ensure!(
                repo.work(path)? == *value,
                "original worktree changed at {path:?}; preserve that edit, then abort this pull and retry"
            );
        }
        Ok(())
    }
}

pub fn state_summary(repo: &Repo) -> Result<Option<serde_json::Value>> {
    let directory = session_dir(repo)?;
    if !directory.exists() {
        return Ok(None);
    }
    let state = read_session(repo)?;
    let checkout = open_checkout(repo)?;
    Ok(Some(serde_json::json!({
        "checkout": checkout.root,
        "original_head": state.head,
        "started_at_unix": state.started_at_unix,
        "operation": operation(&checkout)?,
        "install_interrupted": directory.join("install.json").exists(),
    })))
}

pub fn run(repo: &Repo, args: &[String]) -> Result<i32> {
    repo.assert_attached()?;
    repo.assert_no_sync()?;
    match args.first().map(String::as_str) {
        Some("--continue") => {
            ensure!(args.len() == 1, "pull --continue takes no extra arguments");
            return continue_pull(repo);
        }
        Some("--abort") => {
            ensure!(args.len() == 1, "pull --abort takes no extra arguments");
            return abort_pull(repo);
        }
        Some("--status") => {
            ensure!(args.len() == 1, "pull --status takes no extra arguments");
            return status(repo);
        }
        Some("--git") => {
            ensure!(args.len() > 1, "pull --git requires Git arguments");
            read_session(repo)?;
            return open_checkout(repo)?.passthrough(&args[1..]);
        }
        _ => {}
    }
    let directory = session_dir(repo)?;
    ensure!(
        !directory.exists(),
        "a pull is already pending; inspect it with `overgit pull --status`, continue or abort it"
    );
    let previous = repo.manifest()?;
    ops::validate_overlay(repo, &previous)?;
    let dirty = repo.overlay(["status", "--porcelain", "--untracked-files=no"])?;
    ensure!(
        dirty.is_empty(),
        "overlay has uncommitted changes; commit them before pull"
    );
    let state = Session::capture(repo, &previous)?;
    fs::create_dir_all(&directory)?;
    let setup = temporary_checkout(repo, &directory);
    let checkout = match setup {
        Ok(checkout) => checkout,
        Err(error) => {
            fs::remove_dir_all(&directory)?;
            return Err(error);
        }
    };
    atomic_write(
        &directory.join("session.json"),
        &serde_json::to_vec(&state)?,
    )?;
    let arguments = normalize_arguments(repo, args)?;
    let code = user_git(&checkout, "pull", &arguments)?;
    if code != 0 {
        if operation(&checkout)?.is_some() {
            report_pending(repo, &checkout);
            return Ok(3);
        }
        fs::remove_dir_all(&directory)?;
        return Ok(code);
    }
    complete(repo, &checkout, &state)
}

fn session_dir(repo: &Repo) -> Result<std::path::PathBuf> {
    let directory = repo.local("pull");
    crate::repo::ensure_not_symlink(&directory)?;
    Ok(directory)
}
fn read_session(repo: &Repo) -> Result<Session> {
    let path = session_dir(repo)?.join("session.json");
    crate::repo::ensure_not_symlink(&path)?;
    let state: Session =
        serde_json::from_slice(&fs::read(path).context("no readable pending pull session")?)?;
    ensure!(state.version == 1, "unsupported pending pull version");
    let manifest: Manifest = serde_json::from_slice(&state.manifest)?;
    manifest.validate()?;
    let repo = &repo.with_layout(state.work.keys());
    for (path, value) in &state.work {
        repo.validate_leaf(path)?;
        if let Some(value) = value {
            value.validate()?;
        }
    }
    Ok(state)
}
fn open_checkout(repo: &Repo) -> Result<Repo> {
    let root = session_dir(repo)?.join("tree");
    crate::repo::ensure_not_symlink(&root)?;
    isolated_repo(&root)
}
fn isolated_repo(root: &Path) -> Result<Repo> {
    // A persistent sandbox is nested under .overgit. Ordinary discovery deliberately
    // leaves that directory, so open the explicitly created disposable base directly.
    Repo::isolated(root)
}

fn operation(checkout: &Repo) -> Result<Option<&'static str>> {
    let metadata = overlay_git_dir(checkout)?;
    if metadata.join("rebase-merge").exists() || metadata.join("rebase-apply").exists() {
        return Ok(Some("rebase"));
    }
    if metadata.join("MERGE_HEAD").exists() {
        return Ok(Some("merge"));
    }
    if ["CHERRY_PICK_HEAD", "REVERT_HEAD", "sequencer"]
        .iter()
        .any(|name| metadata.join(name).exists())
    {
        return Ok(Some("manual"));
    }
    let dirty = checkout.overlay(["status", "--porcelain", "--untracked-files=no"])?;
    Ok(if dirty.is_empty() {
        None
    } else {
        Some("changes")
    })
}
fn report_pending(repo: &Repo, checkout: &Repo) {
    crate::say!("pull is pending in {}", checkout.root.display());
    crate::say!(
        "edit files there, stage with `overgit -C {:?} pull --git add <path>`, then `overgit pull --continue`",
        repo.root
    );
    crate::say!(
        "`overgit pull --abort` leaves the original repository unchanged and archives the isolated work"
    );
}
fn user_git(checkout: &Repo, name: &str, arguments: &[String]) -> Result<i32> {
    let mut command = checkout.interactive_cmd();
    command.arg(name);
    if name == "pull" && crate::QUIET.load(Ordering::Relaxed) {
        command.arg("--quiet");
    }
    let status = command
        .args(arguments)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;
    Ok(status.code().unwrap_or(1))
}
fn status(repo: &Repo) -> Result<i32> {
    let summary = state_summary(repo)?.context("no pull is pending")?;
    crate::say!("{}", serde_json::to_string_pretty(&summary)?);
    open_checkout(repo)?.passthrough(&["status".into(), "--short".into()])
}
fn continue_pull(repo: &Repo) -> Result<i32> {
    let state = read_session(repo)?;
    let checkout = open_checkout(repo)?;
    if recover_install(repo)? {
        fs::remove_dir_all(session_dir(repo)?)?;
        return Ok(0);
    }
    state.ensure_unchanged(repo)?;
    let unmerged = checkout.overlay(["ls-files", "--unmerged", "-z"])?;
    if !unmerged.is_empty() {
        report_pending(repo, &checkout);
        return Ok(3);
    }
    let code = match operation(&checkout)? {
        Some("merge") => user_git(&checkout, "commit", &["--no-edit".into()])?,
        Some("rebase") => user_git(&checkout, "rebase", &["--continue".into()])?,
        Some("changes") => user_git(
            &checkout,
            "commit",
            &["-m".into(), "overgit: complete isolated pull".into()],
        )?,
        _ => 0,
    };
    if code != 0 || operation(&checkout)?.is_some() {
        report_pending(repo, &checkout);
        return Ok(3);
    }
    complete(repo, &checkout, &state)
}
fn complete(repo: &Repo, checkout: &Repo, state: &Session) -> Result<i32> {
    if operation(checkout)?.is_some() {
        report_pending(repo, checkout);
        return Ok(3);
    }
    state.ensure_unchanged(repo)?;
    ensure!(
        head_branch(checkout)? == state.branch,
        "isolated checkout changed branch; restore its original branch before continuing"
    );
    let isolated_refs = refs(checkout)?;
    let unchanged_branches = |references: &BTreeMap<String, Reference>| {
        references
            .iter()
            .filter(|(name, _)| {
                name.starts_with("refs/heads/") && Some(name.as_str()) != state.branch.as_deref()
            })
            .map(|(name, value)| (name.clone(), (value.oid.clone(), value.symbolic.clone())))
            .collect::<BTreeMap<_, _>>()
    };
    ensure!(
        unchanged_branches(&state.refs) == unchanged_branches(&isolated_refs),
        "isolated checkout changed unrelated branches; restore them before continuing"
    );
    let value = checkout
        .indexed(MANIFEST, true)?
        .context("pulled overlay has no manifest")?;
    ensure!(
        value.mode == "100644",
        "pulled manifest must be a regular file"
    );
    let manifest: Manifest = serde_json::from_slice(&value.data)?;
    manifest.validate()?;
    ops::validate_overlay(checkout, &manifest)?;
    let previous = repo.manifest()?;
    preflight(repo, &previous, &manifest)?;
    install(repo, checkout, &previous, &value.data, &manifest)?;
    fs::remove_dir_all(session_dir(repo)?)?;
    Ok(0)
}
fn abort_pull(repo: &Repo) -> Result<i32> {
    read_session(repo)?;
    if recover_install(repo)? {
        crate::say!(
            "pull was already installed before interruption; preserving the installed result"
        );
    }
    let parent = repo.local("backups");
    crate::repo::ensure_not_symlink(&parent)?;
    fs::create_dir_all(&parent)?;
    let backup = tempfile::Builder::new()
        .prefix("pull-aborted-")
        .tempdir_in(&parent)?
        .keep();
    fs::remove_dir(&backup)?;
    fs::rename(session_dir(repo)?, &backup)?;
    crate::say!(
        "aborted pull; isolated files preserved in {}",
        backup.display()
    );
    Ok(0)
}
fn index_path(repo: &Repo, overlay: bool) -> Result<std::path::PathBuf> {
    let args = ["rev-parse", "--path-format=absolute", "--git-path", "index"];
    let data = if overlay {
        repo.overlay(args)?
    } else {
        repo.base(args)?
    };
    Ok(git::text(data)?.into())
}
fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn base_head(repo: &Repo) -> Result<Option<String>> {
    let result = git::output(
        repo.base_cmd()
            .args(["rev-parse", "--verify", "--quiet", "HEAD"]),
        None,
    )?;
    if result.status.success() {
        Ok(Some(git::text(result.stdout)?))
    } else {
        ensure!(result.status.code() == Some(1), "cannot read base HEAD");
        Ok(None)
    }
}
fn normalize_arguments(repo: &Repo, args: &[String]) -> Result<Vec<String>> {
    let mut result = args.to_vec();
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            index += 1;
            break;
        }
        if !argument.starts_with('-') {
            break;
        }
        let takes_value = matches!(
            argument.as_str(),
            "--upload-pack"
                | "--server-option"
                | "--depth"
                | "--shallow-since"
                | "--shallow-exclude"
                | "--deepen"
                | "--filter"
                | "--jobs"
                | "-j"
                | "--strategy"
                | "-s"
                | "--strategy-option"
                | "-X"
                | "--refmap"
                | "--negotiation-tip"
                | "--recurse-submodules-default"
        );
        index += if takes_value { 2 } else { 1 };
    }
    if let Some(repository) = result.get_mut(index) {
        let remotes = git::text(repo.overlay(["remote"])?)?;
        if !remotes.lines().any(|name| name == repository)
            && !repository.contains(':')
            && !Path::new(repository).is_absolute()
        {
            repo.cwd
                .join(&*repository)
                .to_str()
                .context("non-UTF-8 repository argument")?
                .clone_into(repository);
        }
    }
    Ok(result)
}

fn temporary_checkout(repo: &Repo, directory: &Path) -> Result<Repo> {
    let root = directory.join("tree");
    fs::create_dir(&root)?;
    // A disposable base lets the normal path validator inspect the temporary overlay.
    // Source base files are never copied, checked out or modified here.
    git::run(&root, ["init", "--quiet"])?;
    git::run(
        &root,
        [
            OsString::from("clone"),
            OsString::from("--shared"),
            OsString::from("--no-checkout"),
            OsString::from("--"),
            repo.overlay_dir().into_os_string(),
            root.join(".overgit").into_os_string(),
        ],
    )?;
    let checkout = isolated_repo(&root)?;
    // clone's origin and tracking refs refer to the source checkout. Replace them with
    // the source's actual configuration and refs before asking Git to pull.
    checkout.overlay(["config", "--remove-section", "remote.origin"])?;
    copy_configuration(repo, &checkout)?;
    for (name, _) in refs(&checkout)? {
        checkout.overlay(["update-ref", "--no-deref", "-d", &name])?;
    }
    let originals = refs(repo)?;
    for (name, reference) in &originals {
        if reference.symbolic.is_empty() {
            checkout.overlay(["update-ref", name, &reference.oid])?;
        }
    }
    for (name, reference) in &originals {
        if !reference.symbolic.is_empty() {
            checkout.overlay(["symbolic-ref", name, &reference.symbolic])?;
        }
    }
    if let Some(branch) = head_branch(repo)? {
        checkout.overlay(["symbolic-ref", "HEAD", &branch])?;
    } else {
        let head = git::text(repo.overlay(["rev-parse", "HEAD"])?)?;
        checkout.overlay(["update-ref", "--no-deref", "HEAD", &head])?;
    }
    checkout.overlay(["reset", "--hard", "HEAD"])?;
    Ok(checkout)
}

fn copy_configuration(source: &Repo, destination: &Repo) -> Result<()> {
    let data = source.overlay(["config", "--null", "--list", "--includes"])?;
    for row in data.split(|byte| *byte == 0).filter(|row| !row.is_empty()) {
        let (key, value) = row
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or((row, &[][..]), |newline| {
                (&row[..newline], &row[newline + 1..])
            });
        let key = std::str::from_utf8(key)?;
        if skip_configuration(key) {
            continue;
        }
        if key.starts_with("remote.")
            && key
                .rsplit_once('.')
                .is_some_and(|(_, field)| matches!(field, "url" | "pushurl"))
        {
            continue;
        }
        destination.overlay(["config", "--add", "--", key, std::str::from_utf8(value)?])?;
    }
    copy_remote_urls(source, destination)?;
    destination.overlay(["config", "core.bare", "false"])?;
    // Pull isolation includes hooks. User hooks can explicitly target the shared tree,
    // so they must not run as an incidental side effect of this disposable checkout.
    destination.overlay(["config", "core.hooksPath", "/dev/null"])?;
    Ok(())
}

fn copy_remote_urls(source: &Repo, destination: &Repo) -> Result<()> {
    let remotes = git::text(source.overlay(["remote"])?)?;
    for remote in remotes.lines() {
        // Let Git expand insteadOf aliases before interpreting relative local paths.
        let urls = git::text(source.overlay(["remote", "get-url", "--all", remote])?)?;
        for url in urls.lines() {
            let url = if !url.contains(':') && !Path::new(url).is_absolute() {
                source
                    .root
                    .join(url)
                    .to_str()
                    .context("non-UTF-8 remote path")?
                    .to_owned()
            } else {
                url.to_owned()
            };
            destination.overlay(["config", "--add", &format!("remote.{remote}.url"), &url])?;
        }
    }
    Ok(())
}

fn skip_configuration(key: &str) -> bool {
    key.starts_with("include.")
        || key.starts_with("includeif.")
        || key.starts_with("extensions.")
        || matches!(
            key,
            "core.repositoryformatversion"
                | "core.bare"
                | "core.worktree"
                | "core.hookspath"
                | "core.fsmonitor"
                | "core.splitindex"
                | "core.sparsecheckout"
                | "core.sparsecheckoutcone"
                | "core.excludesfile"
                | "extensions.worktreeconfig"
                | "extensions.objectformat"
        )
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Reference {
    oid: String,
    symbolic: String,
}
fn refs(repo: &Repo) -> Result<BTreeMap<String, Reference>> {
    let text = git::text(repo.overlay([
        "for-each-ref",
        "--format=%(refname)%09%(objectname)%09%(symref)",
    ])?)?;
    text.lines()
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(fields.len() == 3, "invalid Git ref record");
            Ok((
                fields[0].to_owned(),
                Reference {
                    oid: fields[1].to_owned(),
                    symbolic: fields[2].to_owned(),
                },
            ))
        })
        .collect()
}
fn head_branch(repo: &Repo) -> Result<Option<String>> {
    let output = git::output(
        repo.overlay_cmd().args(["symbolic-ref", "--quiet", "HEAD"]),
        None,
    )?;
    if output.status.success() {
        Ok(Some(git::text(output.stdout)?))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        git::check(&output)?;
        Ok(None)
    }
}
fn overlay_git_dir(repo: &Repo) -> Result<std::path::PathBuf> {
    Ok(git::text(repo.overlay(["rev-parse", "--absolute-git-dir"])?)?.into())
}

fn preflight(repo: &Repo, previous: &Manifest, incoming: &Manifest) -> Result<()> {
    let previous_files = repo.managed_files(previous)?;
    let incoming_files = repo.managed_files(incoming)?;
    let repo = &repo.with_layout(previous_files.keys().chain(incoming_files.keys()));
    for dir in incoming.opaque.difference(&previous.opaque) {
        repo.assert_base_unstaged(dir)?;
    }
    repo.assert_patterns_unstaged(
        incoming,
        incoming
            .patterns
            .difference(&previous.patterns)
            .map(String::as_str),
    )?;
    for (path, kind) in incoming_files {
        if !previous_files.contains_key(&path) {
            repo.staged_base_safe(&path)?;
        }
        // An obstruction must fail before any original refs, indexes or files change.
        repo.work(&path)?;
        if kind == Kind::Add {
            ensure!(
                !path.contains('\n') && !path.ends_with('\r'),
                "added path cannot be represented in Git exclude at {path:?}"
            );
        }
    }
    Ok(())
}

fn install(
    repo: &Repo,
    checkout: &Repo,
    previous: &Manifest,
    bytes: &[u8],
    manifest: &Manifest,
) -> Result<()> {
    let old_refs = refs(repo)?;
    let new_refs = refs(checkout)?;
    let old_head = git::text(repo.overlay(["rev-parse", "HEAD"])?)?;
    let new_head = git::text(checkout.overlay(["rev-parse", "HEAD"])?)?;
    repo.overlay([
        OsString::from("fetch"),
        OsString::from("--quiet"),
        OsString::from("--no-tags"),
        OsString::from("--no-write-fetch-head"),
        OsString::from("--no-auto-maintenance"),
        OsString::from("--"),
        checkout.overlay_dir().into_os_string(),
        OsString::from(&new_head),
    ])?;
    // Import any fetched side refs too, including tags not reachable from the new HEAD.
    for (name, reference) in &new_refs {
        if reference.symbolic.is_empty()
            && old_refs
                .get(name)
                .is_none_or(|old| old.oid != reference.oid)
        {
            repo.overlay([
                OsString::from("fetch"),
                OsString::from("--quiet"),
                OsString::from("--no-tags"),
                OsString::from("--no-write-fetch-head"),
                OsString::from("--no-auto-maintenance"),
                OsString::from("--"),
                checkout.overlay_dir().into_os_string(),
                OsString::from(&reference.oid),
            ])?;
        }
    }
    let recovery = BeforeInstall::capture(repo, previous, manifest, old_head.clone())?;
    let recovery_path = session_dir(repo)?.join("install.json");
    atomic_write(&recovery_path, &serde_json::to_vec(&recovery)?)?;
    if let Err(error) = apply_result(
        repo,
        previous,
        bytes,
        manifest,
        (&old_refs, &new_refs),
        (&old_head, &new_head),
    ) {
        return match recovery.restore(repo) {
            Ok(()) => {
                fs::remove_file(&recovery_path)?;
                Err(error.context("pull could not be installed; original overlay state was restored"))
            },
            Err(rollback) => Err(error.context(format!("pull could not be installed; rollback also failed ({rollback:#}); previous content remains in overlay commit {old_head} and rescued files are under .overgit/local/backups"))),
        };
    }
    fs::remove_file(&recovery_path)?;
    Ok(())
}

fn recover_install(repo: &Repo) -> Result<bool> {
    let path = session_dir(repo)?.join("install.json");
    if !path.exists() {
        return Ok(false);
    }
    crate::repo::ensure_not_symlink(&path)?;
    let recovery: BeforeInstall = serde_json::from_slice(&fs::read(&path)?)?;
    let state = read_session(repo)?;
    let checkout = open_checkout(repo)?;
    let current_head = git::text(repo.overlay(["rev-parse", "HEAD"])?)?;
    let target_head = git::text(checkout.overlay(["rev-parse", "HEAD"])?)?;
    let current_refs = refs(repo)?;
    if current_head == target_head
        && current_refs == refs(&checkout)?
        && (current_head != state.head || current_refs != state.refs)
    {
        // Ref installation is the final transaction. Its completed target proves that
        // files were installed before interruption; preserve any later user edits.
        fs::remove_file(path)?;
        return Ok(true);
    }
    ensure!(
        current_head == recovery.head && current_refs == state.refs,
        "original refs changed after interrupted pull installation; preserve this session before manual recovery"
    );
    recovery.restore(repo)?;
    fs::remove_file(path)?;
    Ok(false)
}

fn apply_result(
    repo: &Repo,
    previous: &Manifest,
    bytes: &[u8],
    manifest: &Manifest,
    refs: (&BTreeMap<String, Reference>, &BTreeMap<String, Reference>),
    heads: (&str, &str),
) -> Result<()> {
    let (old_refs, new_refs) = refs;
    let (old_head, new_head) = heads;
    // The original checkout was clean. Its old content remains recoverable in old HEAD,
    // and every unexpected base-side file is backed up by reconciliation before replacement.
    repo.overlay(["read-tree", new_head])?;
    let incoming = repo.managed_files(manifest)?;
    let old_files = repo.managed_files(previous)?;
    let repo = &repo.with_layout(old_files.keys().chain(incoming.keys()));
    let mut released = Vec::new();
    for path in old_files
        .keys()
        .filter(|path| !incoming.contains_key(*path))
    {
        released.push((path, repo.indexed(path, false)?));
    }
    released.sort_by_key(|(_, base)| base.is_some());
    for (path, base) in released {
        repo.replace(path, base.as_ref(), true, false)?;
        if base.is_none() {
            ops::prune_opaque_parents(repo, previous, path)?;
        }
        repo.skip(path, false)?;
    }
    atomic_write(&repo.root.join(MANIFEST), bytes)?;
    ops::reconcile_pull(repo)?;
    let mut commands = String::from("start\n");
    for (name, reference) in new_refs {
        if !reference.symbolic.is_empty() {
            continue;
        }
        let old = old_refs.get(name).filter(|old| old.symbolic.is_empty());
        if old.is_none_or(|old| old.oid != reference.oid) {
            use std::fmt::Write as _;
            writeln!(
                commands,
                "update {name} {} {}",
                reference.oid,
                old.map_or_else(|| "0".repeat(reference.oid.len()), |old| old.oid.clone())
            )?;
        }
    }
    for (name, reference) in old_refs {
        if reference.symbolic.is_empty() && !new_refs.contains_key(name) {
            use std::fmt::Write as _;
            writeln!(commands, "delete {name} {}", reference.oid)?;
        }
    }
    if head_branch(repo)?.is_none() && new_head != old_head {
        use std::fmt::Write as _;
        writeln!(commands, "update HEAD {new_head} {old_head}")?;
    }
    commands.push_str("prepare\ncommit\n");
    let output = git::output(
        repo.overlay_cmd().args(["update-ref", "--stdin"]),
        Some(commands.as_bytes()),
    )?;
    git::check(&output)?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BeforeInstall {
    head: String,
    manifest: Vec<u8>,
    work: BTreeMap<String, Option<Snapshot>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    directories: BTreeMap<String, bool>,
    overlay_index: Vec<u8>,
    #[serde(default)]
    overlay_shared_indexes: BTreeMap<String, Vec<u8>>,
    base_index: Option<Vec<u8>>,
    #[serde(default)]
    base_shared_indexes: BTreeMap<String, Vec<u8>>,
    base_entries: Vec<u8>,
    exclude: Option<Vec<u8>>,
}
impl BeforeInstall {
    fn capture(
        repo: &Repo,
        previous: &Manifest,
        incoming: &Manifest,
        head: String,
    ) -> Result<Self> {
        let paths: BTreeSet<_> = repo
            .managed_files(previous)?
            .into_keys()
            .chain(repo.managed_files(incoming)?.into_keys())
            .collect();
        let repo = &repo.with_layout(&paths);
        let directories = crate::mutation::directory_state(repo, &paths)?;
        let work = paths
            .into_iter()
            .map(|path| Ok((path.clone(), repo.work(&path)?)))
            .collect::<Result<_>>()?;
        let exclude = match fs::read(repo.common.join("info/exclude")) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            head,
            manifest: fs::read(repo.root.join(MANIFEST))?,
            work,
            directories,
            overlay_index: fs::read(index_path(repo, true)?)?,
            overlay_shared_indexes: shared_indexes(&index_path(repo, true)?)?,
            base_index: optional_bytes(&index_path(repo, false)?)?,
            base_shared_indexes: shared_indexes(&index_path(repo, false)?)?,
            base_entries: repo.base(["ls-files", "--stage", "-z"])?,
            exclude,
        })
    }
    fn restore(self, repo: &Repo) -> Result<()> {
        let repo = &repo.with_layout(self.work.keys());
        let base_lock = index_path(repo, false)?.with_file_name("index.lock");
        crate::repo::ensure_not_symlink(&base_lock)?;
        ensure!(!base_lock.exists(), "base Git index is locked");
        ensure!(
            repo.base(["ls-files", "--stage", "-z"])? == self.base_entries,
            "base index entries changed after pull installation began; preserve the pull recovery session before retrying"
        );
        let manifest: Manifest = serde_json::from_slice(&self.manifest)?;
        manifest.validate()?;
        let protection = ops::exclude_bytes(repo, Some(&manifest), !repo.detached())?;
        let index = index_path(repo, true)?;
        let lock = index.with_file_name("index.lock");
        crate::repo::ensure_not_symlink(&lock)?;
        let _guard = crate::mutation::GitIndexLock::acquire(lock)?;
        restore_shared_indexes(&index, &self.overlay_shared_indexes)?;
        restore_shared_indexes(&index_path(repo, false)?, &self.base_shared_indexes)?;
        let original_flags = saved_base_flags(repo, self.base_index.as_deref(), &self.work)?;
        // External staging after interruption can hold the only copy of new bytes.
        // Rescue payloads before replacing their last index reference. Parsing the
        // index also refuses an unresolved external merge before changing files.
        for path in repo.index(true)?.keys() {
            if let Some(value) = repo.indexed(path, true)? {
                repo.rescue(path, &value)?;
            }
        }
        let manifest_path = repo.root.join(MANIFEST);
        crate::repo::ensure_not_symlink(&manifest_path)?;
        if let Some(current) = crate::repo::read_snapshot(&manifest_path)?
            && (current.data != self.manifest || current.mode != "100644")
        {
            repo.rescue(MANIFEST, &current)?;
        }
        crate::mutation::recover_work(repo, &self.work, &self.directories)?;
        atomic_write(&repo.root.join(MANIFEST), &self.manifest)?;
        atomic_write(&index, &self.overlay_index)?;
        // Restore only owned paths on the current index, preserving unrelated flags
        // even if a Git index lock or another interruption makes recovery retry.
        for (path, flag) in original_flags {
            for option in [
                if flag.eq_ignore_ascii_case(&b'S') {
                    "--skip-worktree"
                } else {
                    "--no-skip-worktree"
                },
                if flag.is_ascii_lowercase() {
                    "--assume-unchanged"
                } else {
                    "--no-assume-unchanged"
                },
            ] {
                repo.base([
                    OsString::from("update-index"),
                    OsString::from(option),
                    OsString::from("--"),
                    path.clone(),
                ])?;
            }
        }
        let exclude = repo.common.join("info/exclude");
        // Other worktrees and manual ignore rules can change while recovery waits.
        atomic_write(&exclude, &protection)?;
        Ok(())
    }
}

fn shared_indexes(index: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut snapshots = BTreeMap::new();
    for entry in fs::read_dir(index.parent().context("index has no parent")?)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with("sharedindex.")
        {
            crate::repo::ensure_not_symlink(&entry.path())?;
            snapshots.insert(name.to_owned(), fs::read(entry.path())?);
        }
    }
    Ok(snapshots)
}

fn restore_shared_indexes(index: &Path, snapshots: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let directory = index.parent().context("index has no parent")?;
    for (name, bytes) in snapshots {
        ensure!(
            name.starts_with("sharedindex.") && !name.contains('/'),
            "invalid shared index snapshot name"
        );
        let path = directory.join(name);
        crate::repo::ensure_not_symlink(&path)?;
        if !path.try_exists()? {
            atomic_write(&path, bytes)?;
        }
    }
    Ok(())
}

fn saved_base_flags(
    repo: &Repo,
    bytes: Option<&[u8]>,
    work: &BTreeMap<String, Option<Snapshot>>,
) -> Result<Vec<(OsString, u8)>> {
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    let index = index_path(repo, false)?;
    let temporary =
        tempfile::NamedTempFile::new_in(index.parent().context("index has no parent")?)?;
    fs::write(temporary.path(), bytes)?;
    let output = git::output(
        repo.base_cmd()
            .env("GIT_INDEX_FILE", temporary.path())
            .args(["ls-files", "-v", "-z"]),
        None,
    )?;
    git::check(&output)?;
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let path = record
                .get(2..)
                .and_then(|path| std::str::from_utf8(path).ok())?;
            work.contains_key(path).then_some((path, record))
        })
        .map(|(path, record)| {
            ensure!(
                record.len() >= 3 && record[1] == b' ',
                "invalid base flag record"
            );
            Ok((OsString::from(path), record[0]))
        })
        .collect()
}
