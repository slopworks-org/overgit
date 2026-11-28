mod base;
mod bootstrap;
mod doctor;
mod git;
mod inspect;
mod inspection;
mod model;
mod mutation;
mod ops;
mod pull;
mod recovery;
mod repo;
mod sync;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand};
use repo::Repo;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

static QUIET: AtomicBool = AtomicBool::new(false);
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => { if !$crate::QUIET.load(std::sync::atomic::Ordering::Relaxed) { println!($($arg)*); } };
}

pub(crate) const VERSION: &str = match option_env!("OVERGIT_BUILD_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(
    version = VERSION,
    about = "An independent Git overlay in a shared working tree",
    long_about = "An independent Git overlay in a shared working tree\n\nStart in a Git worktree: overgit init, overgit add <path>, overgit commit -m\n<message>.\nUpdate the base with overgit base pull. Run overgit sync to merge upstream\nchanges into owned files.\nUse overgit <command> --help for workflows and safety details.\nExit codes: 0 success, 1 failure, 2 usage, 3 pending sync/pull, 4 attention\nneeded.\nGit passthrough commands retain Git exit codes."
)]
struct Cli {
    #[arg(short = 'C', default_value = ".", help = "Run from this directory")]
    working_directory: PathBuf,
    #[arg(short, long, help = "Suppress progress and report output")]
    quiet: bool,
    #[arg(long,  value_parser = ["auto", "always", "never"], default_value = "auto", help = "Accepted for scripts; reports use plain text")]
    color: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create an empty overlay in the current base repository
    #[command(
        long_about = "Create an empty overlay in the current base repository\n\nRun inside an existing base Git worktree. One overlay per worktree; bare\nrepositories, sparse checkout and submodule overlays are unsupported. The\noverlay lives in .overgit and is excluded from base Git.\n\nExample:\n  overgit init --remote <overlay-url>\n  overgit add config.toml personal.sh\n  overgit commit -m \"personal setup\"\n  overgit push -u origin main\n\nOverrides and single-file whiteouts include original base contents in overlay\nhistory. Share the overlay only with people allowed to read those contents.\nKeep important overlay commits pushed; git clean -xffd can delete .overgit and\nlocal recovery data."
    )]
    Init {
        #[arg(long, help = "Set the overlay origin URL")]
        remote: Option<String>,
        #[arg(long, default_value = "main", help = "Initial overlay branch")]
        branch: String,
    },
    /// Clone an overlay, optionally cloning its base first
    #[command(
        long_about = "Clone an overlay, optionally cloning its base first\n\nWith --base, clone the base into DIRECTORY before installing the overlay.\nWithout --base, run inside an existing base worktree. Repeating the same\noverlay remote and branch is safe.\n\nExamples:\n  overgit clone <overlay-url> --base <base-url> project\n  overgit clone <overlay-url>\n\nThe overlay contains original base snapshots as well as your changes. Only\nshare it with recipients permitted to read that base data."
    )]
    Clone {
        /// Overlay repository URL
        url: String,
        /// Destination for a newly cloned base
        directory: Option<PathBuf>,
        #[arg(long, help = "Clone this base repository first")]
        base: Option<String>,
        #[arg(long, help = "Select the overlay branch")]
        branch: Option<String>,
    },
    /// Own files or recursive directories and immediately stage their content
    #[command(
        long_about = "Own files or recursive directories and immediately stage their content\n\nPaths are relative to the invocation directory. Directories are recursive;\nregular files, executable files and symlink leaves are supported. Existing\nownership is skipped unless --force is given. Base-tracked files become\noverrides, other files become additions. After rm DIR, create your replacement\nfiles and add them here. Other base files under DIR stay hidden.\n\nStaged base changes at target paths must be resolved first. Symlink parents,\nnested repositories, non-UTF-8 ownership and case/Unicode-equivalent aliases\nare rejected. Added paths cannot contain newline or end in CR. Reserved aliases\nare ignored by base Git, including in sibling worktrees."
    )]
    Add {
        #[arg(
            short,
            long,
            help = "Restage already owned paths from current working files"
        )]
        force: bool,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Hide base files, make directories opaque, add pattern masks, or remove overlay additions
    #[command(
        long_about = "Hide base files, make directories opaque, add pattern masks, or remove overlay additions\n\nA base file becomes a whiteout. Removing a directory makes it opaque. --pattern\nrecords a gitignore-style glob rule (patterns without '/' match filenames at any\ndepth, e.g. SPEC.md; patterns with '/' match repository-relative paths, with\n'**', '*', '?' and '[...]' supported). Attach, apply, and sync hide matching\nbase files, including new files from upstream. Untracked files are preserved.\n\nTo replace a project directory with your own files:\n  overgit rm .agents\n  # Create your replacement files in .agents/\n  overgit add .agents\n  overgit commit -m \"replace project agents\"\n\nTo hide every SPEC.md tracked by the base anywhere in the repository:\n  overgit rm --pattern SPEC.md\n  overgit commit -m \"hide spec files\"\n\nRemoving a replacement hides the base file again. Use overgit restore .agents\nor overgit restore --pattern SPEC.md to remove a rule and restore base files."
    )]
    Rm {
        #[arg(short, long, help = "Back up uncommitted content before removing it")]
        force: bool,
        #[arg(
            short = 'p',
            long = "pattern",
            value_name = "PATTERN",
            help = "Hide tracked base files matching a gitignore-style glob pattern"
        )]
        patterns: Vec<String>,
        #[arg(required_unless_present = "patterns")]
        paths: Vec<PathBuf>,
    },
    /// Give paths or pattern masks back to the base
    #[command(
        long_about = "Give paths or pattern masks back to the base\n\nRelinquishes ownership and restores base content. Overlay additions are removed\nunless --keep-file is given. To release an opaque directory and its replacements, run\n  overgit restore .agents\nTo remove a pattern mask and restore base files hidden only by it, run\n  overgit restore --pattern SPEC.md\nExplicitly owned entries matching the pattern stay owned. Restoring one\nreplacement under an active mask leaves the mask in effect.\nFor a backup, use overgit recovery restore."
    )]
    Restore {
        #[arg(short, long, help = "Back up uncommitted content before replacing it")]
        force: bool,
        #[arg(
            long,
            conflicts_with_all = ["paths", "patterns"],
            help = "Relinquish all overlay ownership"
        )]
        all: bool,
        #[arg(
            long,
            help = "Keep overlay additions in the working tree unless a mask hides the base file"
        )]
        keep_file: bool,
        #[arg(
            short = 'p',
            long = "pattern",
            value_name = "PATTERN",
            help = "Remove a pattern rule and restore matching base files"
        )]
        patterns: Vec<String>,
        #[arg(required_unless_present_any = ["all", "patterns"])]
        paths: Vec<PathBuf>,
    },
    /// List paths and rules owned by the overlay
    List {
        #[arg(long, value_parser = ["add", "override", "delete", "opaque", "pattern"])]
        /// Filter by ownership kind
        kind: Option<String>,
        #[arg(long)]
        /// Emit tab-separated, NUL-terminated records with the path last
        porcelain: bool,
    },
    /// Report ownership; exit 1 if any path is outside the overlay
    #[command(visible_alias = "inspect")]
    Which {
        #[arg(long)]
        /// Emit tab-separated, NUL-terminated records with the path last
        porcelain: bool,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Show both repositories and local overlay health; porcelain is JSON
    Status {
        #[arg(short, long, conflicts_with = "porcelain")]
        /// Omit branch and summary lines
        short: bool,
        #[arg(long, conflicts_with = "porcelain")]
        /// Include unchanged owned paths
        all: bool,
        #[arg(long)]
        /// Emit versioned JSON for both repositories and diagnostics
        porcelain: bool,
    },
    /// Record current owned content; missing files retain their saved content
    #[command(
        long_about = "Record current owned content; missing files retain their saved content\n\nBy default, stages current existing owned files before committing. Missing\nowned files retain their saved content; use rm for intentional deletion.\nWithout -m, Git opens its configured editor. Use --no-all to commit only the\nexisting overlay index. Finish pending sync before committing."
    )]
    Commit {
        #[arg(short, long, help = "Commit message; omit to open the Git editor")]
        message: Option<String>,
        #[arg(long, help = "Replace the last overlay commit")]
        amend: bool,
        #[arg(long, help = "Commit the existing index without staging working files")]
        no_all: bool,
    },
    /// Show an overlay diff using Git options
    #[command(
        long_about = "Show an overlay diff using Git options\n\nArguments are forwarded to git diff against the overlay. Use -- before Git\narguments to forward help, for example overgit diff -- --help. Git exit status\nis preserved."
    )]
    Diff {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show overlay history using Git options
    #[command(
        long_about = "Show overlay history using Git options\n\nArguments are forwarded to git log against the overlay. Use -- before Git\narguments to forward help, for example overgit log -- --help. Git exit status\nis preserved."
    )]
    Log {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Push the overlay using Git options
    #[command(
        long_about = "Push the overlay using Git options\n\nArguments are forwarded to git push. Use -- before Git arguments to forward\nhelp, for example overgit push -- --help. Git exit status is preserved.\n\nThe overlay includes complete original base snapshots for overrides and\nwhiteouts. Push only to recipients permitted to read those files, even if your\ncurrent overlay hides or redacts them."
    )]
    Push {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Fetch the overlay using Git options
    #[command(
        long_about = "Fetch the overlay using Git options\n\nArguments are forwarded to git fetch against the overlay. Use -- before Git\narguments to forward help, for example overgit fetch -- --help. Git exit status\nis preserved."
    )]
    Fetch {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Pull into a private worktree, then safely apply the result
    #[command(
        long_about = "Pull into a private worktree, then safely apply the result\n\nCommit overlay changes first. Git pull arguments run in an isolated checkout\nwith user hooks disabled; the base is never pulled. Use -- --help to forward\nGit help.\n\nSession commands:\n  overgit pull --status       Show the pending pull\n  overgit pull --git <args>   Run Git in the isolated overlay\n  overgit pull --continue     Install the resolved result\n  overgit pull --abort        Archive isolated edits and end the session\n\nPending merge/rebase/squash work exits 3 and stays in .overgit/local/pull/tree.\nEdit files there, stage with pull --git add <path>, then continue. The original\nproject is unchanged while pending. If the original project changes, preserve\nthat work, abort and retry. Aborted edits remain available through recovery\nlist."
    )]
    Pull {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Git against the base, detaching and reattaching for worktree updates
    #[command(
        long_about = "Run Git against the base repository\n\nOvergit detaches for pull, merge, rebase, checkout, switch, reset, cherry-pick,\nrevert, stash, restore, bisect, am, apply, clean, mv and rm. Dry runs of clean,\nmv and rm pass through, as do read-only patch reports, help, stash list and\nstash show. Git aliases use their target's rule. Detach manually before shell\naliases or external commands that rewrite owned paths.\n\n  overgit base pull\n  overgit base rebase --continue\n  overgit base -- --help\n\nDuring a conflict the overlay stays detached. Finish or abort with overgit base\nto reattach, or run overgit attach once Git is idle. If Git leaves base edits on\nowned paths, review and commit or discard them with overgit base to reattach.\nA manual detach stays in effect. Run overgit sync to merge upstream changes.\nClean refuses force flags that put .overgit at risk. Use one -f, and on older\nGit set -c clean.requireForce=true before clean."
    )]
    Base {
        /// Git options, subcommand and arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        git_args: Vec<std::ffi::OsString>,
    },
    /// Run arbitrary Git against the overlay, bypassing overgit safeguards
    #[command(
        long_about = "Run arbitrary Git against the overlay, bypassing overgit safeguards\n\nRuns Git directly against the overlay and can modify its manifest, index and\nworking files outside overgit safeguards. Keep a recoverable commit first.\nArguments after the Git subcommand, including --help, belong to Git. For\nexample overgit git status --help. Exit status is preserved."
    )]
    Git {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Rebuild overlay files and local Git state; attach remounts a detached overlay
    #[command(alias = "attach")]
    #[command(
        long_about = "Rebuild overlay files and local Git state; attach remounts a detached overlay\n\nAlso available as attach. Restores saved overlay bytes and local Git state.\nAttach also hides current base files under opaque directories or delete patterns.\nModified owned files are backed up before replacement. Use doctor --fix to\nrepair drift while preserving ordinary edits.\n\nBase update workflow:\n  overgit detach\n  git pull\n  overgit attach\n  overgit sync\n\nGit clean -xfd or stash -a can remove additions; apply restores saved content.\nGit clean -xffd can delete the overlay itself and its backups."
    )]
    Apply {
        #[arg(
            short = 'n',
            long,
            help = "Preview without writing; exit 4 if changes are needed"
        )]
        dry_run: bool,
        #[arg(
            short,
            long,
            help = "Move obstructing directories into recovery backups when attached without masks"
        )]
        force: bool,
    },
    /// Save current overlay content and restore pristine base files
    #[command(
        long_about = "Save current overlay content and restore pristine base files\n\nSaves current overlay edits, then restores base index content so base Git can\nupdate normally. Known pristine base drift does not replace saved overlay\ncontent; use add --force to make that replacement intentional. Unrelated base\nedits are preserved.\n\nRun detach, then the base Git operation, then attach and sync. Hooks do not\ndetach for you. Finish or abort base merge/rebase/cherry-pick/revert/bisect\noperations before mutating the overlay."
    )]
    Detach {
        #[arg(
            short,
            long,
            help = "Rescue unexpected whiteout content and directory obstructions"
        )]
        force: bool,
    },
    /// Three-way merge current base revisions into the overlay
    #[command(
        long_about = "Three-way merge current base revisions into the overlay\n\nAfter detach, updating base Git, and attach, merge current base HEAD changes\ninto owned files. Sync hides new base files under opaque directories or delete\npatterns and merges changes to explicit replacements. Text conflicts get markers;\nbinary, symlink, deletion and addition collisions require explicit decisions.\n\n  overgit sync --dry-run\n  overgit sync\n  overgit resolve <path>          After editing conflict markers\n  overgit resolve --keep <path>   Keep your version\n  overgit sync --continue\n\nUse resolve --help for other decisions. --abort restores pre-sync state and\nrescues edits made during resolution. Exit 3 means pending decisions; exit 4\ncan mean missing local inputs, requiring apply first. Finish or abort sync\nbefore commit, detach, apply or pull."
    )]
    Sync {
        #[arg(short = 'n', long, conflicts_with_all = ["cont", "abort"])]
        /// Preview merge decisions without writing
        dry_run: bool,
        #[arg(long = "continue", conflicts_with = "abort")]
        /// Finish a sync after all paths have been resolved
        cont: bool,
        #[arg(long)]
        /// Restore pre-sync state, rescuing newer edits first
        abort: bool,
        #[arg(long, conflicts_with_all = ["cont", "abort"])]
        /// Sync one file or opaque directory; repeat for multiple paths
        only: Vec<PathBuf>,
        #[arg(long, conflicts_with_all = ["dry_run", "cont", "abort", "only"])]
        /// Print JSON snapshots for a pending path decision
        inspect: Option<PathBuf>,
        #[arg(long, default_value = "merge", value_parser = ["merge", "diff3", "zdiff3"])]
        /// Conflict marker style
        style: String,
    },
    /// Resolve sync conflicts or explicitly choose an ownership decision
    #[command(
        long_about = "Resolve sync conflicts or explicitly choose an ownership decision\n\nWithout a decision flag, accept your edited text after removing conflict\nmarkers. --adopt handles a newly base-tracked overlay addition and may itself\nproduce conflicts. --keep turns an upstream-deleted override into an addition.\n--drop and --take-upstream relinquish ownership, rescuing discarded content.\n\nAfter resolving all paths, run overgit sync --continue. Exit 3 means other\ndecisions remain pending."
    )]
    Resolve {
        #[arg(long, conflicts_with_all = ["drop", "adopt", "take_upstream"])]
        /// Keep overlay content
        keep: bool,
        #[arg(long, conflicts_with_all = ["adopt", "take_upstream"])]
        /// Relinquish ownership
        drop: bool,
        #[arg(long, conflicts_with = "take_upstream")]
        /// Merge an addition collision against an empty fork
        adopt: bool,
        #[arg(long)]
        /// Relinquish ownership and take the base version
        take_upstream: bool,
        #[arg(short, long)]
        /// Accept edited content even if conflict markers remain
        force: bool,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Diagnose local state; fix repairs safe drift while retaining normal edits
    #[command(
        long_about = "Diagnose local state; fix repairs safe drift while retaining normal edits\n\nUse doctor --fix after an interrupted mutation. Safe repairs preserve normal\nlocal edits; apply instead restores saved bytes. Recovery refuses to overwrite\na changed HEAD or unresolved external overlay merge.\n\n--rebuild explicitly reconstructs lost ownership and resets unknown fork\npoints. Recovering whiteouts, opaque directories and pattern rules requires a\nsurviving manifest or a copy from overlay history. Recovery entries are\nreported without reading or validating payloads; inspect them with recovery\ninspect. Warnings exit 4; informational context does not fail the command."
    )]
    Doctor {
        #[arg(long, help = "Repair safe drift and recover interrupted mutations")]
        fix: bool,
        #[arg(
            long,
            requires = "fix",
            help = "Infer lost ownership from overlay files and reset unknown fork points against the current base"
        )]
        rebuild: bool,
        #[arg(long)]
        /// Emit a JSON array of diagnostics with severity and recovery hints
        porcelain: bool,
    },
    /// Browse saved recovery data and restore selected working files
    #[command(
        long_about = "Browse saved recovery data and restore selected working files\n\nSaved data is local to this worktree in .overgit/local/backups.\n\n  overgit recovery list\n  overgit recovery inspect <id>\n  overgit recovery restore <id> --dry-run\n  overgit recovery restore <id>\n\nUse the exact ID from list. Directory rescues and aborted pulls require --path\nto select a leaf. Inspection remains available during pending operations or a\ndamaged overlay index. Whole-directory restoration, archived-session resumption\nand backup deletion are not provided."
    )]
    Recovery {
        #[command(subcommand)]
        action: RecoveryCommand,
    },
    /// Manage opt-in base hooks that reapply the overlay
    #[command(
        long_about = "Manage opt-in base hooks that reapply the overlay\n\nOpt-in hooks reapply the overlay after base checkout, merge and rewrite, before\nexisting shell hook code. Your hook runs last and can change files. An apply\nfailure does not prevent it from running. Foreign-language hooks are skipped.\nHooks never detach before a base operation; use detach, update, attach, sync\nfor base updates."
    )]
    Hooks {
        #[command(subcommand)]
        action: HookCommand,
    },
    /// Manage paired linked worktrees for the base and overlay
    #[command(
        long_about = "Manage paired linked worktrees for the base and overlay\n\nPaired worktrees share Git history but use independent branches and local\nrecovery data. Added paths are excluded by the union of mounted overlays, so\nthe same paths are also ignored in sibling base worktrees."
    )]
    Worktree {
        #[command(subcommand)]
        action: WorktreeCommand,
    },
    /// Update overgit to the latest build on the configured update channel
    #[command(
        long_about = "Update overgit to the latest build on the configured update channel\n\nDownloads the latest verified release archive from GitHub, checks its SHA-256\nchecksum and reported version, and atomically replaces the running executable.\n\nChannels:\n  main    Latest builds from the main branch (default)\n\nExamples:\n  overgit update                  Install the latest build on the saved channel\n  overgit update --check          Check for updates without installing\n  overgit update --channel main   Select the main channel and update\n  overgit update --force          Reinstall even if already up to date\n\nThe selected channel is stored in $XDG_CONFIG_HOME/overgit/update-channel\n(or $HOME/.config/overgit/update-channel). With --check, --channel inspects\nthat channel without changing the saved setting. Requires curl and tar."
    )]
    Update {
        /// Check for updates without installing or changing settings
        #[arg(long)]
        check: bool,
        /// Select the update channel (saved unless --check is set)
        #[arg(long, value_enum)]
        channel: Option<update::Channel>,
        /// Reinstall even if already up to date
        #[arg(short, long, conflicts_with = "check")]
        force: bool,
    },
}
#[derive(Subcommand)]
enum HookCommand {
    /// Install managed blocks without replacing existing shell hook code
    Install,
    /// Remove only overgit-managed blocks
    Uninstall,
}
#[derive(Subcommand)]
enum RecoveryCommand {
    /// List local recovery entries without reading their payloads
    List {
        #[arg(long)]
        /// Emit versioned JSON
        porcelain: bool,
    },
    /// Preview one saved snapshot or an archive's immediate children
    #[command(
        long_about = "Preview one saved snapshot or an archive's immediate children\n\nPreview bytes are bounded and escaped. Directories show immediate children; use\n--path to select a saved file or symlink. Unsupported or incomplete entries are\nnot assumed restorable."
    )]
    Inspect {
        /// Exact entry ID from recovery list
        id: String,
        #[arg(long)]
        /// Leaf relative to the rescued directory or archived project tree
        path: Option<String>,
        #[arg(long)]
        /// Emit versioned JSON
        porcelain: bool,
    },
    /// Restore a saved leaf to the worktree, preserving indexes and ownership
    #[command(
        long_about = "Restore a saved leaf to the worktree, preserving indexes and ownership\n\nChanges only the working file, backing up differing current content first. Both\nindexes, ownership and the source backup stay unchanged. Identical content is a\nno-op.\n\nUse --dry-run first. It exits 4 for a change and 0 for a no-op; restoration\nexits 0 on success. --to is relative to the invocation directory; a recorded\noriginal path is relative to the project root. Metadata snapshots require --to\nfor extraction into an ordinary project file.\n\nRestoration refuses directory obstructions, whiteout destinations, unsafe\nparent links, submodules and active sync/pull/mutation recovery. After\ninterruption, use doctor --fix. A restored unowned path is visible to base Git.\nTo save a restored owned file, use add --force followed by commit."
    )]
    Restore {
        /// Exact entry ID from recovery list
        id: String,
        #[arg(long)]
        /// Leaf relative to the rescued directory or archived project tree
        path: Option<String>,
        /// Extract to this project path instead of the recorded original path
        #[arg(long)]
        to: Option<PathBuf>,
        /// Validate and preview only; exit 4 if a file would change
        #[arg(short = 'n', long)]
        dry_run: bool,
        #[arg(long)]
        /// Emit versioned JSON
        porcelain: bool,
    },
}
#[derive(Subcommand)]
enum WorktreeCommand {
    /// Create a paired base and overlay worktree
    Add {
        path: PathBuf,
        commit: Option<String>,
        #[arg(short = 'b', long)]
        /// Create this base branch
        branch: Option<String>,
        #[arg(long)]
        /// Defaults to the base branch, or directory name for detached HEAD
        overlay_branch: Option<String>,
    },
    /// List paired worktrees
    List,
    /// Remove a paired worktree; move recovery backups elsewhere first
    Remove {
        path: PathBuf,
        #[arg(short, long)]
        /// Allow removal with dirty files, pending operations or backups; data can be lost
        force: bool,
    },
}
fn main() {
    let cli = Cli::parse();
    QUIET.store(cli.quiet, Ordering::Relaxed);
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("overgit: {error:#}");
            std::process::exit(1);
        }
    }
}
fn run(cli: Cli) -> Result<i32> {
    let cwd = cli.working_directory.canonicalize()?;
    match &cli.command {
        Command::Update {
            check,
            channel,
            force,
        } => return update::run(*check, *channel, *force),
        Command::Base { git_args } => return base::run(&cwd, git_args),
        Command::Init { remote, branch } => {
            return bootstrap::init(&cwd, remote.as_deref(), branch);
        }
        Command::Clone {
            url,
            directory,
            base,
            branch,
        } => {
            return bootstrap::clone_overlay(
                &cwd,
                url,
                directory.as_deref(),
                base.as_deref(),
                branch.as_deref(),
            );
        }
        _ => {}
    }
    let repo = Repo::discover(&cwd)?;
    if let Command::Recovery { action } = &cli.command {
        return recovery::run(&repo, action);
    }
    if let Command::Worktree {
        action: WorktreeCommand::List,
    } = &cli.command
    {
        return bootstrap::worktree_list(&repo);
    }
    if let Command::Doctor {
        fix,
        rebuild,
        porcelain,
    } = &cli.command
    {
        let _lock = if *fix { Some(repo.lock()?) } else { None };
        return doctor::run(&repo, *fix, *rebuild, *porcelain);
    }
    repo.require_overlay()?;
    match cli.command {
        Command::List { kind, porcelain } => inspect::list(&repo, kind.as_deref(), porcelain),
        Command::Which { paths, porcelain } => inspect::which(&repo, &paths, porcelain),
        Command::Status {
            short,
            all,
            porcelain,
        } => inspect::status(&repo, short, all, porcelain),
        Command::Diff { args } => pass(&repo, "diff", args),
        Command::Log { args } => pass(&repo, "log", args),
        Command::Fetch { args } => {
            let _lock = repo.lock()?;
            pass(&repo, "fetch", args)
        }
        Command::Push { args } => {
            let _lock = repo.lock()?;
            pass(&repo, "push", args)
        }
        Command::Git { args } => {
            let _lock = repo.lock()?;
            repo.passthrough(&args)
        }
        Command::Doctor {
            fix,
            rebuild,
            porcelain,
        } => {
            let _lock = if fix { Some(repo.lock()?) } else { None };
            doctor::run(&repo, fix, rebuild, porcelain)
        }
        command => {
            let _lock = repo.lock()?;
            repo.assert_idle()?;
            mutation::assert_clear(&repo)?;
            mutate(&repo, command, cli.quiet)
        }
    }
}
fn mutate(repo: &Repo, command: Command, quiet: bool) -> Result<i32> {
    match command {
        Command::Add { paths, force } => ops::add(repo, &paths, force),
        Command::Rm {
            paths,
            patterns,
            force,
        } => ops::remove(repo, &paths, &patterns, force, ops::Removal::Whiteout),
        Command::Restore {
            paths,
            patterns,
            force,
            all,
            keep_file,
        } => ops::remove(
            repo,
            &paths,
            &patterns,
            force,
            ops::Removal::Restore { all, keep_file },
        ),
        Command::Commit {
            message,
            amend,
            no_all,
        } => {
            repo.assert_attached()?;
            repo.assert_no_sync()?;
            if !no_all {
                ops::stage_owned(repo)?;
            }
            let mut args = vec!["commit".into()];
            if let Some(message) = message {
                args.extend(["-m".into(), message]);
            }
            if amend {
                args.push("--amend".into());
            }
            if quiet {
                args.push("--quiet".into());
            }
            repo.passthrough(&args)
        }
        Command::Apply { dry_run, force } => {
            repo.assert_no_sync()?;
            Ok(if ops::reconcile(repo, dry_run, force)? && dry_run {
                4
            } else {
                0
            })
        }
        Command::Detach { force } => ops::detach(repo, force, false),
        command @ (Command::Sync { .. } | Command::Resolve { .. }) => synchronize(repo, command),
        Command::Pull { args } => pull::run(repo, &args),
        Command::Hooks { action } => bootstrap::hooks(repo, matches!(action, HookCommand::Install)),
        Command::Worktree { action } => match action {
            WorktreeCommand::Add {
                path,
                commit,
                branch,
                overlay_branch,
            } => bootstrap::worktree_add(
                repo,
                &path,
                commit.as_deref(),
                branch.as_deref(),
                overlay_branch.as_deref(),
            ),
            WorktreeCommand::Remove { path, force } => {
                bootstrap::worktree_remove(repo, &path, force)
            }
            WorktreeCommand::List => bootstrap::worktree_list(repo),
        },
        _ => anyhow::bail!("unexpected command dispatch"),
    }
}

fn pass(repo: &Repo, name: &str, args: Vec<String>) -> Result<i32> {
    let mut command = vec![name.to_owned()];
    command.extend(args);
    repo.passthrough(&command)
}
fn synchronize(repo: &Repo, command: Command) -> Result<i32> {
    repo.assert_attached()?;
    match command {
        Command::Sync {
            dry_run,
            cont,
            abort,
            only,
            style,
            inspect,
        } => {
            repo.assert_attached()?;
            if let Some(path) = inspect {
                sync::inspect(repo, &repo.path(&path)?)
            } else if abort {
                sync::abort(repo)
            } else if cont {
                sync::continue_sync(repo)
            } else {
                let only = only
                    .iter()
                    .map(|path| repo.path(path))
                    .collect::<Result<Vec<_>>>()?;
                sync::run(repo, &only, &style, dry_run)
            }
        }
        Command::Resolve {
            keep,
            drop,
            adopt,
            take_upstream,
            force,
            paths,
        } => {
            repo.assert_attached()?;
            let paths = paths
                .iter()
                .map(|path| repo.path(path))
                .collect::<Result<Vec<_>>>()?;
            let decision = if keep {
                Some("keep")
            } else if drop {
                Some("drop")
            } else if adopt {
                Some("adopt")
            } else if take_upstream {
                Some("take-upstream")
            } else {
                None
            };
            sync::resolve(repo, &paths, decision, force)
        }
        _ => anyhow::bail!("unexpected sync dispatch"),
    }
}

#[cfg(test)]
mod help_tests {
    use super::{Cli, Command};
    use clap::Parser as _;

    #[test]
    fn git_arguments_keep_their_help_and_option_meanings() -> anyhow::Result<()> {
        assert!(Cli::try_parse_from(["overgit", "base", "-C", "dir", "pull", "--help"]).is_ok());
        for command in ["diff", "log", "push", "fetch", "pull", "git"] {
            for args in [
                vec!["--", "--help"],
                vec!["status", "--help"],
                vec!["--quiet", "--help"],
            ] {
                let mut input = vec!["overgit", command];
                input.extend(args.iter().copied());
                let parsed = Cli::try_parse_from(input)?;
                let (Command::Diff { args: forwarded }
                | Command::Log { args: forwarded }
                | Command::Push { args: forwarded }
                | Command::Fetch { args: forwarded }
                | Command::Pull { args: forwarded }
                | Command::Git { args: forwarded }) = parsed.command
                else {
                    anyhow::bail!("unexpected command");
                };
                let expected = if args[0] == "--" {
                    &args[1..]
                } else {
                    &args[..]
                };
                assert_eq!(forwarded, expected);
            }
        }
        for args in [
            vec!["--status"],
            vec!["--continue"],
            vec!["--abort"],
            vec!["--git", "status", "--help"],
        ] {
            let mut input = vec!["overgit", "pull"];
            input.extend(args.iter().copied());
            let Command::Pull { args: forwarded } = Cli::try_parse_from(input)?.command else {
                anyhow::bail!("unexpected command");
            };
            assert_eq!(forwarded, args);
        }
        Ok(())
    }
}
