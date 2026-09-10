use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
};

// Repository-local variables reported by `git rev-parse --local-env-vars`,
// excluding command-line configuration transport such as GIT_CONFIG_COUNT.
// These must not redirect Git commands intended for a newly-created worktree.
const REPOSITORY_ENVIRONMENT: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

enum GitContext {
    Source,
    WorktreeMutation,
    Destination,
}

fn output_with_context(cwd: &Path, args: &[&str], context: GitContext) -> Result<Output> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);

    match context {
        GitContext::Source => {}
        GitContext::WorktreeMutation => {
            // Add/remove select the source repository, but Git's internal
            // checkout/status must use the target worktree's own index.
            command.env_remove("GIT_INDEX_FILE");
        }
        GitContext::Destination => {
            for name in REPOSITORY_ENVIRONMENT {
                command.env_remove(name);
            }
        }
    }

    command.output().context("could not execute git")
}

pub fn output(cwd: &Path, args: &[&str]) -> Result<Output> {
    output_with_context(cwd, args, GitContext::Source)
}

fn checked(out: Output, args: &[&str]) -> Result<Vec<u8>> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);

        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_owned()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_owned()
        } else {
            out.status.to_string()
        };
        bail!("git {}: {}", args.first().unwrap_or(&""), detail);
    }

    if !out.stderr.is_empty() {
        io::stderr()
            .write_all(&out.stderr)
            .context("writing Git diagnostics")?;
    }

    Ok(out.stdout)
}

pub fn run(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(output(cwd, args)?, args)
}

pub fn run_worktree_mutation(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(
        output_with_context(cwd, args, GitContext::WorktreeMutation)?,
        args,
    )
}

pub fn run_in_worktree(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(
        output_with_context(cwd, args, GitContext::Destination)?,
        args,
    )
}

pub fn text(cwd: &Path, args: &[&str]) -> Result<String> {
    let bytes = run(cwd, args)?;
    let value = String::from_utf8(bytes).context("Git returned non-UTF-8 data")?;
    Ok(value.strip_suffix('\n').unwrap_or(&value).to_owned())
}

pub fn text_in_worktree(cwd: &Path, args: &[&str]) -> Result<String> {
    let bytes = run_in_worktree(cwd, args)?;
    let value = String::from_utf8(bytes).context("Git returned non-UTF-8 data")?;
    Ok(value.strip_suffix('\n').unwrap_or(&value).to_owned())
}

pub fn optional(cwd: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = output(cwd, args)?;
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    if !out.status.success() {
        bail!(
            "git lookup failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let value = String::from_utf8(out.stdout).context("Git returned non-UTF-8 data")?;
    Ok(Some(value.strip_suffix('\n').unwrap_or(&value).to_owned()))
}

/// Validate a selected checkout itself, not an enclosing repository discovered by Git.
pub fn validate_worktree(source_cwd: &Path, path: &Path) -> Result<()> {
    let top =
        PathBuf::from(text_in_worktree(path, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
    if top != path.canonicalize()? {
        bail!(
            "Git discovered a different working tree at {}",
            top.display()
        );
    }

    let args = &["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let expected = PathBuf::from(text(source_cwd, args)?).canonicalize()?;
    let actual = PathBuf::from(text_in_worktree(path, args)?).canonicalize()?;
    if actual != expected {
        bail!("selected working tree belongs to a different repository");
    }

    Ok(())
}

pub fn nul_paths(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    bytes
        .split(|b| *b == 0)
        .filter(|v| !v.is_empty())
        .map(|v| {
            Ok(PathBuf::from(
                std::str::from_utf8(v).context("non-UTF-8 Git path")?,
            ))
        })
        .collect()
}

pub fn path_str(p: &Path) -> Result<&str> {
    p.to_str().context("non-UTF-8 path")
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub current: bool,
    pub primary: bool,
    pub detached: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prune_reason: Option<String>,
    #[serde(skip)]
    pub bare: bool,
}

pub fn inventory(cwd: &Path) -> Result<Vec<Worktree>> {
    let bytes = run(cwd, &["worktree", "list", "--porcelain", "-z"])?;

    let mut trees = Vec::new();
    let mut current = Worktree::default();
    for bytes in bytes.split(|b| *b == 0) {
        let field = std::str::from_utf8(bytes).context("non-UTF-8 worktree record")?;
        let (name, value) = match field.split_once(' ') {
            Some((name, value)) => (name, Some(value)),
            None => (field, None),
        };

        match (name, value) {
            ("", None) => {
                if !current.path.as_os_str().is_empty() {
                    trees.push(std::mem::take(&mut current));
                }
            }
            ("worktree", Some(path)) => current.path = path.into(),
            ("HEAD", Some(head)) => current.head = Some(head.into()),
            ("branch", Some(reference)) => {
                if let Some(branch) = reference.strip_prefix("refs/heads/") {
                    current.branch = Some(branch.into());
                }
            }
            ("detached", None) => current.detached = true,
            ("bare", None) => current.bare = true,
            ("locked", reason) => {
                current.locked = true;
                current.lock_reason = reason.map(str::to_owned);
            }
            ("prunable", reason) => {
                current.prunable = true;
                current.prune_reason = reason.map(str::to_owned);
            }
            _ => {}
        }
    }

    if let Some(first) = trees.first_mut() {
        first.primary = !first.bare;
    }

    Ok(trees)
}
