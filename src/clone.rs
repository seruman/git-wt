use crate::{git, native};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

pub fn probe(source_directory: &Path, destination_parent: &Path) -> io::Result<()> {
    let source = tempfile::Builder::new()
        .prefix(".git-wt-probe-")
        .tempfile_in(source_directory)?;
    source.as_file().set_len(4096)?;

    let target = tempfile::Builder::new()
        .prefix(".git-wt-probe-")
        .tempdir_in(destination_parent)?;

    native::clone_file(source.path(), &target.path().join("clone"))?;
    Ok(())
}

#[derive(Debug)]
pub struct Entry {
    pub relative: PathBuf,
    pub kind: fs::FileType,
    // Only ignored entries without tracked/nonignored contents may disappear.
    pub disposable: bool,
    // Saved only for directories: disappearing sources must not leave retained
    // contents under the destination's potentially more permissive default mode.
    pub directory_permissions: Option<fs::Permissions>,
}

#[derive(Default, Debug, Serialize)]
pub struct Stats {
    pub files: u64,
    pub logical_bytes: u64,
    pub symlinks: u64,
}

pub fn exclusions(values: Vec<String>) -> Result<Vec<PathBuf>> {
    values
        .into_iter()
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute()
                || p.components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
            {
                bail!("exclusions must be source-relative paths without parent traversal");
            }

            let p: PathBuf = p
                .components()
                .filter(|p| !matches!(p, std::path::Component::CurDir))
                .collect();
            if p.as_os_str().is_empty() {
                bail!("exclusion cannot be empty or the source root");
            }

            git::path_str(&p)?;
            Ok(p)
        })
        .collect()
}

pub fn plan(
    git_cwd: &Path,
    source: &Path,
    dirty: bool,
    exclude: &[PathBuf],
    nested: &[PathBuf],
    destination: &Path,
) -> Result<Vec<Entry>> {
    // Keep source Git commands in the original invocation directory. Relative
    // GIT_DIR/GIT_WORK_TREE selectors are interpreted relative to that directory.
    let tracked: HashSet<PathBuf> = git::nul_paths(&git::run(
        git_cwd,
        &["ls-files", "--cached", "--full-name", "-z", "--", ":/"],
    )?)?
    .into_iter()
    .collect();

    let mut protected = tracked.clone();
    if dirty {
        protected.extend(git::nul_paths(&git::run(
            git_cwd,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "--full-name",
                "-z",
                "--",
                ":/",
            ],
        )?)?);
    }

    let ignored = git::nul_paths(&git::run(
        git_cwd,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--full-name",
            "-z",
            "--",
            ":/",
        ],
    )?)?;

    let roots = if dirty {
        vec![PathBuf::new()]
    } else {
        ignored.clone()
    };

    let ignored: HashSet<PathBuf> = ignored.into_iter().collect();
    let excluded_roots: HashSet<&Path> = exclude.iter().map(PathBuf::as_path).collect();

    let protected_dirs: HashSet<PathBuf> = protected
        .iter()
        .flat_map(|p| p.ancestors().skip(1).map(Path::to_owned))
        .collect();
    let disposable = |relative: &Path| {
        relative.ancestors().any(|p| ignored.contains(p))
            && !protected.contains(relative)
            && !protected_dirs.contains(relative)
    };

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let mut walker = walkdir::WalkDir::new(source.join(&root))
            .follow_links(false)
            .follow_root_links(false)
            .into_iter();
        while let Some(entry) = walker.next() {
            if let Err(error) = &entry
                && let (Some(path), Some(io_error)) = (error.path(), error.io_error())
                && path.strip_prefix(source).is_ok_and(&disposable)
                && disappeared(io_error, path, true)
            {
                continue;
            }

            let entry = entry.context("reading source tree")?;
            let rel = entry.path().strip_prefix(source)?;
            git::path_str(rel)?;
            if rel.as_os_str().is_empty() {
                continue;
            }

            let is_dir = entry.file_type().is_dir();
            let administrative = rel.components().any(|p| p.as_os_str() == ".git");
            let other_tree = nested.iter().any(|p| entry.path().starts_with(p));
            if administrative || other_tree || entry.path().starts_with(destination) {
                if is_dir {
                    walker.skip_current_dir();
                }
                continue;
            }

            if is_dir
                && (entry.path().join(".git").symlink_metadata().is_ok()
                    || (entry.path().join("HEAD").is_file()
                        && entry.path().join("objects").is_dir()
                        && entry.path().join("refs").is_dir()))
            {
                if dirty {
                    bail!(
                        "dirty creation cannot copy nested repository {}",
                        entry.path().display()
                    );
                }

                walker.skip_current_dir();
                continue;
            }

            if !dirty && tracked.contains(rel) {
                continue;
            }

            let is_ignored = rel.ancestors().any(|p| ignored.contains(p));
            let excluded = rel.ancestors().any(|p| excluded_roots.contains(p));
            let protected_here = if is_dir {
                protected_dirs.contains(rel)
            } else {
                protected.contains(rel)
            };
            if is_ignored && excluded && !protected_here {
                if is_dir {
                    walker.skip_current_dir();
                }
                continue;
            }

            if !seen.insert(rel.to_owned()) {
                continue;
            }

            let directory_permissions = if is_dir {
                let metadata = match fs::symlink_metadata(entry.path()) {
                    Ok(metadata) => metadata,
                    Err(error) if disappeared(&error, entry.path(), disposable(rel)) => {
                        walker.skip_current_dir();
                        continue;
                    }
                    Err(error) => {
                        return Err(error).context("planning source directory permissions");
                    }
                };
                if !metadata.is_dir() {
                    bail!(
                        "source is no longer a directory: {}",
                        entry.path().display()
                    );
                }

                Some(metadata.permissions())
            } else {
                None
            };

            entries.push(Entry {
                relative: rel.to_owned(),
                kind: entry.file_type(),
                disposable: disposable(rel),
                directory_permissions,
            });
        }
    }

    entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(entries)
}

// ENOENT can also mean the destination parent vanished. Confirm that the source
// itself is gone; symlink_metadata keeps dangling symlinks distinct from absence.
fn disappeared(error: &io::Error, source: &Path, disposable: bool) -> bool {
    disposable
        && error.kind() == io::ErrorKind::NotFound
        && fs::symlink_metadata(source).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Create missing parents without traversing a destination symlink or file.
fn directories(destination: &Path, rel: &Path, created: &mut Vec<PathBuf>) -> io::Result<bool> {
    let mut current = destination.to_owned();
    for component in rel.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                created.push(current.clone());
            }
            Err(e) => return Err(e),
        }
    }

    Ok(true)
}

pub fn populate(source: &Path, destination: &Path, entries: &[Entry]) -> Result<Stats> {
    let source_root = native::SourceRoot::new(source)?;

    let mut stats = Stats::default();
    let mut dirs = Vec::new();
    for entry in entries {
        let src = source.join(&entry.relative);
        let dst = destination.join(&entry.relative);

        if entry.kind.is_dir() {
            directories(destination, &entry.relative, &mut dirs)?;
            continue;
        }
        if !directories(
            destination,
            entry.relative.parent().unwrap_or(Path::new("")),
            &mut dirs,
        )? {
            continue;
        }

        match fs::symlink_metadata(&dst) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        if entry.kind.is_symlink() {
            let target = match fs::read_link(&src) {
                Ok(target) => target,
                Err(error) if disappeared(&error, &src, entry.disposable) => continue,
                Err(error) => return Err(error).context("reading source symlink"),
            };
            let target = native::retarget(&target, &source_root, destination).unwrap_or(target);

            match std::os::unix::fs::symlink(target, &dst) {
                Ok(()) => stats.symlinks += 1,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).context("creating symlink"),
            }
        } else if entry.kind.is_file() {
            match native::clone_file(&src, &dst) {
                Ok(size) => {
                    stats.files += 1;
                    stats.logical_bytes += size;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) if disappeared(&e, &src, entry.disposable) => {}
                Err(e) => return Err(e).with_context(|| format!("cloning {}", src.display())),
            }
        } else {
            writeln!(
                io::stderr(),
                "git-wt: skipping special file {}",
                src.display()
            )
            .context("writing clone diagnostics")?;
        }
    }

    restore_directory_permissions(source, destination, entries, &dirs)?;
    Ok(stats)
}

// Apply directory modes after population, so read-only source directories are usable.
fn restore_directory_permissions(
    source: &Path,
    destination: &Path,
    entries: &[Entry],
    dirs: &[PathBuf],
) -> Result<()> {
    let disposable_directories: HashMap<&Path, &fs::Permissions> = entries
        .iter()
        .filter(|entry| entry.disposable)
        .filter_map(|entry| {
            entry
                .directory_permissions
                .as_ref()
                .map(|permissions| (entry.relative.as_path(), permissions))
        })
        .collect();

    for dir in dirs.iter().rev() {
        let rel = dir.strip_prefix(destination)?;
        let src = source.join(rel);
        let permissions = match fs::symlink_metadata(&src) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    bail!("source is no longer a directory: {}", src.display());
                }

                metadata.permissions()
            }
            Err(error) if disappeared(&error, &src, disposable_directories.contains_key(rel)) => {
                disposable_directories[rel].clone()
            }
            Err(error) => return Err(error).context("reading source directory permissions"),
        };

        fs::set_permissions(dir, permissions)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn disappearing_directory_preserves_permissions_on_already_cloned_contents() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let source_cache = source.join("cache");
        let destination_cache = destination.join("cache");

        fs::create_dir_all(&source_cache).unwrap();
        fs::set_permissions(&source_cache, fs::Permissions::from_mode(0o700)).unwrap();

        let metadata = fs::symlink_metadata(&source_cache).unwrap();
        let entry = Entry {
            relative: "cache".into(),
            kind: metadata.file_type(),
            disposable: true,
            directory_permissions: Some(metadata.permissions()),
        };

        let credential = source_cache.join("credentials");
        fs::write(&credential, "private sentinel").unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o644)).unwrap();

        fs::create_dir_all(&destination_cache).unwrap();
        fs::set_permissions(&destination_cache, fs::Permissions::from_mode(0o755)).unwrap();
        native::clone_file(&credential, &destination_cache.join("credentials")).unwrap();

        // Establish the exact failure window without a concurrent process:
        // contents are cloned, but the source is gone before mode restoration.
        fs::remove_dir_all(&source_cache).unwrap();
        restore_directory_permissions(
            &source,
            &destination,
            &[entry],
            std::slice::from_ref(&destination_cache),
        )
        .unwrap();

        assert_eq!(
            fs::metadata(&destination_cache)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let cloned = destination_cache.join("credentials");
        assert_eq!(fs::read_to_string(&cloned).unwrap(), "private sentinel");
        assert_eq!(
            fs::metadata(cloned).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn missing_sources_are_only_skipped_when_disposable() {
        for kind in ["file", "symlink", "directory"] {
            for disposable in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let source = root.path().join("source");
                let destination = root.path().join("destination");
                fs::create_dir(&source).unwrap();
                fs::create_dir(&destination).unwrap();

                let path = source.join("entry");
                match kind {
                    "file" => fs::write(&path, "contents").unwrap(),
                    "symlink" => symlink("missing", &path).unwrap(),
                    _ => fs::create_dir(&path).unwrap(),
                }

                let metadata = fs::symlink_metadata(&path).unwrap();
                let entry = Entry {
                    relative: "entry".into(),
                    kind: metadata.file_type(),
                    disposable,
                    directory_permissions: metadata.is_dir().then(|| metadata.permissions()),
                };

                if kind == "directory" {
                    fs::remove_dir(&path).unwrap();
                } else {
                    fs::remove_file(&path).unwrap();
                }

                let result = populate(&source, &destination, &[entry]);
                assert_eq!(result.is_ok(), disposable, "{kind}: {result:?}");
            }
        }
    }

    #[test]
    fn missing_destination_and_changed_source_types_still_fail() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source).unwrap();

        let path = source.join("entry");
        fs::write(&path, "contents").unwrap();
        let entry = Entry {
            relative: "entry".into(),
            kind: fs::symlink_metadata(&path).unwrap().file_type(),
            disposable: true,
            directory_permissions: None,
        };

        // ENOENT from clonefile must not be swallowed when the source exists.
        let error = populate(&source, &destination, &[entry]).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::NotFound
        );

        fs::create_dir(&destination).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let entry = Entry {
            relative: "entry".into(),
            kind: fs::symlink_metadata(&path).unwrap().file_type(),
            disposable: true,
            directory_permissions: Some(fs::metadata(&path).unwrap().permissions()),
        };

        fs::remove_dir(&path).unwrap();
        symlink(&destination, &path).unwrap();
        assert!(populate(&source, &destination, &[entry]).is_err());

        let missing = io::Error::from(io::ErrorKind::NotFound);
        assert!(!disappeared(&missing, &path, true));

        fs::remove_file(&path).unwrap();
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!disappeared(&denied, &path, true));
    }
}
