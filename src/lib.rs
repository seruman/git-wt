mod clone;
mod git;
pub mod naming;
mod native;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use git::path_str;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(message: &str) -> anyhow::Error {
    UsageError(message.into()).into()
}

#[derive(Parser, Debug)]
#[command(name = "git-wt", version, about = "Git worktrees with APFS clones")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create a worktree and clone ignored environment files
    New {
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long, conflicts_with = "no_clone")]
        dirty: bool,
        #[arg(long)]
        exclude: Vec<String>,
        #[arg(long)]
        no_clone: bool,
        #[arg(long)]
        json: bool,
    },

    /// List worktrees registered with Git
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },

    /// Print a worktree's absolute path
    Path {
        worktree: Option<String>,
        #[arg(long, conflicts_with = "worktree")]
        main: bool,
        #[arg(long)]
        json: bool,
    },

    /// Remove a worktree through Git, retaining its branch
    #[command(visible_alias = "rm")]
    Remove {
        worktree: String,
        #[arg(long)]
        force: bool,
    },
}

struct Reservation {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Reservation {
    fn create(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;

        fs::create_dir(path).context("reserving destination")?;
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn still_owns_directory(&self) -> Result<bool> {
        use std::os::unix::fs::MetadataExt;

        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => Ok(metadata.is_dir()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn contains_worktree_metadata(&self) -> Result<bool> {
        if !self.still_owns_directory()? {
            return Ok(false);
        }

        match fs::symlink_metadata(self.path.join(".git")) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

fn source(cwd: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git::text(
        cwd,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )?))
}

fn commit(cwd: &Path, reference: &str) -> Result<String> {
    git::text(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
}

fn branch_exists(cwd: &Path, branch: &str) -> Result<bool> {
    Ok(git::optional(
        cwd,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?
    .is_some())
}

fn exists(p: &Path) -> Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn select<'a>(trees: &'a [git::Worktree], selector: &str, cwd: &Path) -> Result<&'a git::Worktree> {
    // A literal branch name need not also be a traversable filesystem path.
    // Still detect ambiguity whenever both interpretations resolve.
    let path = naming::selector_path(Path::new(selector), cwd);
    let mut matches = trees.iter().filter(|t| {
        t.branch.as_deref() == Some(selector) || path.as_ref().is_ok_and(|p| t.path == *p)
    });

    let found = matches.next();
    if matches.next().is_some() {
        bail!("ambiguous worktree selector; use an absolute path");
    }

    match found {
        Some(tree) => Ok(tree),
        None => {
            path?;
            bail!("no registered worktree matches that branch or path");
        }
    }
}

fn formatted_path(path: &Path, json: bool) -> Result<String> {
    Ok(if json {
        format!("{}\n", serde_json::json!({"path": path_str(path)?}))
    } else {
        format!("{}\n", path_str(path)?)
    })
}

pub fn execute(cli: Cli) -> Result<String> {
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("invalid working directory")?;
    path_str(&cwd)?;

    match cli.command {
        Commands::New {
            branch,
            base,
            path,
            dirty,
            exclude,
            no_clone,
            json,
        } => {
            if dirty && no_clone {
                return Err(usage("--dirty conflicts with --no-clone"));
            }

            new(
                &cwd,
                NewOptions {
                    branch,
                    base,
                    path,
                    dirty,
                    exclude,
                    no_clone,
                    json,
                },
            )
        }
        command => {
            let current = source(&cwd).ok();
            let mut trees = git::inventory(&cwd)?;

            // Git's inventory can report the administrative directory for a main
            // checkout created with --separate-git-dir. When invoked in that main
            // checkout, Git can resolve its real root without a private registry.
            if let Some(root) = &current {
                let git_dir = git::text(&cwd, &["rev-parse", "--absolute-git-dir"])?;
                let common = git::text(
                    &cwd,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )?;

                if git_dir == common
                    && let Some(primary) = trees.iter_mut().find(|t| t.primary)
                {
                    primary.path = root.clone();
                }
            }

            for tree in &mut trees {
                tree.current = current.as_ref() == Some(&tree.path);
            }

            match command {
                Commands::List { json } => {
                    if json {
                        return Ok(format!("{}\n", serde_json::to_string(&trees)?));
                    }

                    let path_width = trees
                        .iter()
                        .map(|tree| Ok(path_str(&tree.path)?.chars().count()))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .max()
                        .unwrap_or(0);

                    let kind_width = trees
                        .iter()
                        .map(|tree| {
                            if tree.branch.is_some() {
                                "branch"
                            } else if tree.bare {
                                "bare"
                            } else {
                                "detached"
                            }
                            .chars()
                            .count()
                        })
                        .max()
                        .unwrap_or(0);

                    let branch_width = trees
                        .iter()
                        .map(|tree| tree.branch.as_deref().unwrap_or("-").chars().count())
                        .max()
                        .unwrap_or(0);

                    let mut out = String::new();
                    for tree in trees {
                        let path = path_str(&tree.path)?;
                        let head = tree
                            .head
                            .as_deref()
                            .map(|head| &head[..head.len().min(8)])
                            .unwrap_or("--------");

                        let kind = if tree.branch.is_some() {
                            "branch"
                        } else if tree.bare {
                            "bare"
                        } else {
                            "detached"
                        };
                        let branch = tree.branch.as_deref().unwrap_or("-");

                        let status = match (tree.locked, tree.prunable) {
                            (false, false) => "-",
                            (true, false) => "locked",
                            (false, true) => "prunable",
                            (true, true) => "locked,prunable",
                        };

                        out.push_str(&format!(
                            "{} {path:<path_width$} {head} {kind:<kind_width$} {branch:<branch_width$} {status}\n",
                            if tree.current { "*" } else { " " },
                        ));
                    }

                    Ok(out)
                }

                Commands::Path {
                    worktree,
                    main,
                    json,
                } => {
                    if main && worktree.is_some() {
                        return Err(usage("--main conflicts with a selector"));
                    }

                    let tree = if main {
                        trees
                            .iter()
                            .find(|t| t.primary)
                            .context("no primary working tree")?
                    } else if let Some(selector) = worktree {
                        select(&trees, &selector, &cwd)?
                    } else {
                        trees
                            .iter()
                            .find(|t| t.current)
                            .context("not inside a working tree")?
                    };

                    git::validate_worktree(&cwd, &tree.path).with_context(|| {
                        format!(
                            "Git did not resolve an accessible working tree at {}",
                            tree.path.display()
                        )
                    })?;

                    formatted_path(&tree.path, json)
                }

                Commands::Remove { worktree, force } => {
                    let tree = select(&trees, &worktree, &cwd)?;
                    if tree.primary || tree.current || tree.bare {
                        bail!("cannot remove the primary or current worktree");
                    }

                    let mut args = vec!["worktree", "remove"];
                    if force {
                        args.push("--force");
                    }
                    args.extend(["--", path_str(&tree.path)?]);

                    git::run_worktree_mutation(&cwd, &args)?;
                    Ok(String::new())
                }

                Commands::New { .. } => unreachable!(),
            }
        }
    }
}

struct NewOptions {
    branch: Option<String>,
    base: Option<String>,
    path: Option<PathBuf>,
    dirty: bool,
    exclude: Vec<String>,
    no_clone: bool,
    json: bool,
}

fn new(cwd: &Path, options: NewOptions) -> Result<String> {
    let src = source(cwd)?;
    let head = commit(cwd, "HEAD").context("creation requires a committed HEAD")?;

    let parent = if options.path.is_none() {
        Some(naming::parent(cwd)?)
    } else {
        None
    };
    let destination_for = |branch: &str| -> Result<PathBuf> {
        match &options.path {
            Some(p) => naming::absolute(p, cwd),
            None => naming::absolute(&parent.as_ref().unwrap().join(branch), cwd),
        }
    };

    let branch = if let Some(branch) = options.branch {
        branch
    } else {
        let source_ref = git::optional(cwd, &["symbolic-ref", "--quiet", "HEAD"])?;
        let source_branch = match source_ref.as_deref() {
            Some(reference) => reference
                .strip_prefix("refs/heads/")
                .context("HEAD does not name a local branch")?,
            None => "detached",
        };

        let local_branches = git::text(
            cwd,
            &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
        )?;

        let mut n: u64 = 1;
        loop {
            let name = format!("{source_branch}-{n}");
            let destination = destination_for(&name)?;
            if options.path.is_some() && exists(&destination)? {
                bail!("destination already exists: {}", destination.display());
            }

            let ref_collision = local_branches.lines().any(|branch| {
                branch == name
                    || branch.starts_with(&format!("{name}/"))
                    || name.starts_with(&format!("{branch}/"))
            });
            if !ref_collision && !exists(&destination)? {
                break name;
            }

            n = n
                .checked_add(1)
                .context("automatic branch names exhausted")?;
        }
    };

    git::run(cwd, &["check-ref-format", "--branch", &branch])
        .map_err(|e| usage(&format!("invalid branch: {e}")))?;

    // check-ref-format accepts @{-n} expansion; git-wt accepts literal local branch names only.
    if branch.starts_with('-') || branch.contains("@{") {
        return Err(usage("branch must be a literal local branch name"));
    }

    let existing = branch_exists(cwd, &branch)?;
    if existing && options.base.is_some() {
        return Err(usage("--base requires a new branch"));
    }

    let start = if existing {
        commit(cwd, &format!("refs/heads/{branch}"))?
    } else if let Some(base) = options.base {
        commit(cwd, &base)?
    } else {
        head.clone()
    };
    if options.dirty && start != head {
        return Err(usage("--dirty requires the source HEAD as starting commit"));
    }

    let dst = destination_for(&branch)?;
    if let Some(parent) = &parent
        && !dst.starts_with(parent)
    {
        bail!("destination escapes configured worktree parent");
    }
    if exists(&dst)? {
        bail!("destination already exists: {}", dst.display());
    }

    let mut excludes = git::optional(cwd, &["config", "--null", "--get-all", "gwt.exclude"])?
        .map(|s| {
            s.split('\0')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    excludes.extend(options.exclude);
    let excludes = clone::exclusions(excludes).map_err(|e| usage(&e.to_string()))?;

    let nested: Vec<PathBuf> = git::inventory(cwd)?
        .into_iter()
        .map(|t| t.path)
        .filter(|p| p != &src && p.starts_with(&src))
        .collect();

    let entries = if options.no_clone {
        Vec::new()
    } else {
        clone::plan(cwd, &src, options.dirty, &excludes, &nested, &dst)?
    };

    let dst_parent = dst.parent().context("destination needs a parent")?;
    fs::create_dir_all(dst_parent)?;

    if !options.no_clone {
        clone::probe(&src, dst_parent)
            .context("APFS cloning unavailable; use --no-clone for an ordinary worktree")?;
    }

    let reservation = Reservation::create(&dst)?;
    let mut owns_worktree = false;

    let operation = (|| -> Result<clone::Stats> {
        let mut args = vec!["worktree", "add"];
        if options.dirty {
            args.push("--no-checkout");
        }
        if !existing {
            args.extend(["-b", &branch]);
        }

        args.extend(["--", path_str(&dst)?]);
        let reference = if existing {
            branch.as_str()
        } else {
            start.as_str()
        };
        args.push(reference);

        if let Err(error) = git::run_worktree_mutation(cwd, &args) {
            // Git can register a worktree before a checkout hook reports failure.
            // Only claim it when metadata appeared inside our reserved directory.
            match reservation.contains_worktree_metadata() {
                Ok(owned) => owns_worktree = owned,
                Err(inspection) => {
                    return Err(error.context(format!(
                        "inspecting destination ownership failed: {inspection:#}"
                    )));
                }
            }

            return Err(error);
        }
        owns_worktree = true;

        let stats = if options.no_clone {
            clone::Stats::default()
        } else {
            clone::populate(&src, &dst, &entries)?
        };

        if options.dirty {
            git::run_in_worktree(&dst, &["reset", "--mixed", &start])?;
        }

        Ok(stats)
    })();

    let stats = match operation {
        Ok(stats) => stats,
        Err(error) => {
            let cleanup = (|| -> Result<()> {
                if !reservation.still_owns_directory()? {
                    bail!("destination ownership changed; refusing cleanup");
                }

                if owns_worktree {
                    git::run_worktree_mutation(
                        cwd,
                        &["worktree", "remove", "--force", "--", path_str(&dst)?],
                    )?;
                } else {
                    fs::remove_dir(&dst).context("removing reserved directory")?;
                }

                Ok(())
            })();

            let branch_note = if !existing && branch_exists(cwd, &branch).unwrap_or(false) {
                format!("; new branch {branch} retained")
            } else {
                String::new()
            };

            match cleanup {
                Ok(()) => bail!("creation failed: {error:#}{branch_note}"),
                Err(cleanup) => bail!(
                    "creation failed: {error:#}{branch_note}; cleanup failed for {}: {cleanup:#}",
                    dst.display()
                ),
            }
        }
    };

    if options.json {
        Ok(format!(
            "{}\n",
            serde_json::json!({
                "path": path_str(&dst)?,
                "branch": branch,
                "head": start,
                "source": path_str(&src)?,

                "mode": if options.no_clone {
                    "none"
                } else if options.dirty {
                    "dirty"
                } else {
                    "ignored"
                },
                "clone": stats
            })
        ))
    } else {
        formatted_path(&dst, false)
    }
}
