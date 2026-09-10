//! Remote naming adapted from ghq.
//! https://github.com/x-motemen/ghq/tree/2474201bccd2cd8c206127630d49b8ce2044dddf

/*
The MIT License (MIT)

Copyright (c) 2014 motemen

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use crate::git;
use anyhow::{Context, Result, bail};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

pub fn remote_path(raw: &str) -> Result<(String, PathBuf)> {
    let normalized = if raw.contains("://") {
        raw.to_owned()
    } else if let Some((host, path)) = raw.split_once(':') {
        if host.contains('/') || host.is_empty() {
            bail!("local remote needs --path");
        }

        format!("ssh://{host}/{}", path.trim_start_matches('/'))
    } else {
        bail!("remote has no hostname; supply --path");
    };

    let url = url::Url::parse(&normalized).context("invalid remote URL; supply --path")?;
    if !matches!(url.scheme(), "ssh" | "https" | "http" | "git") {
        bail!("remote URL requires --path");
    }

    let host = url
        .host_str()
        .context("remote has no hostname; supply --path")?;

    let decoded = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .context("non-UTF-8 remote path")?;

    let path = decoded.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() {
        bail!("remote has no repository path; supply --path");
    }

    let rel = Path::new(path);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("invalid remote repository path");
    }

    Ok((normalized, Path::new(host).join(rel)))
}

pub fn parent(cwd: &Path) -> Result<PathBuf> {
    let remote =
        git::optional(cwd, &["config", "--get", "gwt.remote"])?.unwrap_or_else(|| "origin".into());
    let url = git::text(cwd, &["remote", "get-url", "--", &remote])
        .context("cannot resolve repository remote; supply --path")?;
    let (url, repo) = remote_path(&url)?;

    let root = git::optional(
        cwd,
        &["config", "--path", "--get-urlmatch", "gwt.root", &url],
    )?
    .map(PathBuf::from)
    .unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("worktrees")
    });

    if !root.is_absolute() {
        bail!("gwt.root must be absolute (~/ is supported by Git)");
    }

    // This is a directory boundary, not the final entry being reserved.
    // Resolve its own symlink too, so containment uses one physical namespace.
    let parent = resolve_directory(&root.join(repo))?;
    git::path_str(&parent)?;
    Ok(parent)
}

/// Resolve a directory physically, allowing only unambiguous missing descendants.
/// Never collapse `..` before resolving symlinks: it can name another directory.
fn resolve_directory(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(resolved) => {
            if !fs::metadata(&resolved)?.is_dir() {
                bail!("path ancestor is not a directory: {}", path.display());
            }

            Ok(resolved)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::symlink_metadata(path) {
                Ok(_) => return Err(error).context("resolving path ancestor"),
                Err(metadata) if metadata.kind() == io::ErrorKind::NotFound => {}
                Err(metadata) => return Err(metadata).context("reading path ancestor"),
            }

            // A missing prefix followed by `..` has no filesystem identity.
            // Refuse it instead of guessing by removing the missing component.
            let name = path
                .file_name()
                .context("cannot resolve parent traversal through a missing directory")?;
            let parent = path.parent().context("no accessible path ancestor")?;
            Ok(resolve_directory(parent)?.join(name))
        }
        Err(error) => Err(error).context("resolving path ancestor"),
    }
}

/// Existing selectors identify the physical worktree, including a final alias.
/// Missing paths can still identify a stale registered worktree for removal.
pub fn selector_path(path: &Path, cwd: &Path) -> Result<PathBuf> {
    let absolute = absolute(path, cwd)?;

    match absolute.canonicalize() {
        Ok(resolved) => {
            git::path_str(&resolved)?;
            Ok(resolved)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(absolute),
        Err(error) => Err(error).context("resolving worktree selector"),
    }
}

/// Resolve existing ancestors, preserving the final entry for collision checks.
pub fn absolute(path: &Path, cwd: &Path) -> Result<PathBuf> {
    git::path_str(path)?;
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };

    let result = match joined.file_name() {
        Some(name) => {
            resolve_directory(joined.parent().context("destination needs a parent")?)?.join(name)
        }
        None => resolve_directory(&joined)?,
    };

    git::path_str(&result)?;
    Ok(result)
}
