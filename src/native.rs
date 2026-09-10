//! APFS copy-on-write cloning and symlink retargeting adapted from Lane.
//! https://github.com/lukeed/lane/tree/b3d81607126ab60f0efdab03b8ba9ba42449fec2

/*
MIT License

Copyright (c) Luke Edwards <luke.edwards05@gmail.com> (lukeed.com)

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
*/

#[cfg(not(target_os = "macos"))]
compile_error!("git-wt supports macOS only");

use std::{
    ffi::CString,
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

// clonefile(2) reports these when the source and destination cannot be cloned.
const UNSUPPORTED: &[i32] = &[
    libc::ENOTSUP,
    libc::EOPNOTSUPP,
    libc::ENOTTY,
    libc::EXDEV,
    libc::EINVAL,
    libc::EPERM,
    libc::ENOSYS,
];

const CLONE_NOFOLLOW: u32 = 1;

/// An absolute link into the source tree must point into the clone instead.
pub fn retarget(link: &Path, source_roots: &[PathBuf], destination: &Path) -> Option<PathBuf> {
    // Parent traversal can cross a symlink boundary or escape the source.
    // Preserve its stored spelling, even for apparently internal `..`, rather
    // than guessing containment or requiring dangling targets to exist.
    if link
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return None;
    }

    source_roots
        .iter()
        .find_map(|root| link.strip_prefix(root).ok())
        .map(|relative| destination.join(relative))
}

pub fn root_spellings(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut roots = vec![root.to_path_buf()];
    let canonical = root.canonicalize()?;
    if canonical != root {
        roots.push(canonical.clone());
    }

    // Git reports /private paths even when symlinks retain the user's shorter spelling.
    if let Ok(relative) = canonical.strip_prefix("/private") {
        let alias = Path::new("/").join(relative);
        if !roots.contains(&alias) && alias.canonicalize().ok().as_ref() == Some(&canonical) {
            roots.push(alias);
        }
    }

    Ok(roots)
}

/// Clone one regular file with APFS copy-on-write semantics, preserving its mode.
/// A source type change is an error; the caller owns cleanup of the cloned entry.
pub fn clone_file(source: &Path, destination: &Path) -> io::Result<u64> {
    let c_source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let c_destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;

    // CLONE_NOFOLLOW clones a symlink itself rather than its target. Population handles
    // symlinks separately, but keeping this flag makes the primitive safe on its own.
    if unsafe { libc::clonefile(c_source.as_ptr(), c_destination.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        let metadata = fs::symlink_metadata(destination)?;
        if !metadata.is_file() {
            return Err(io::Error::other(
                "cloned object is no longer a regular file",
            ));
        }

        return Ok(metadata.len());
    }

    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| UNSUPPORTED.contains(&code))
    {
        Err(io::Error::new(io::ErrorKind::Unsupported, error))
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::clone_file;
    use std::{fs, io, os::unix::fs::PermissionsExt};

    #[test]
    fn clone_is_independent_preserves_mode_and_refuses_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("clone");

        let data = vec![42; 16_384];
        fs::write(&source, &data).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            clone_file(&source, &destination)
                .expect("tests must run on an APFS volume with clonefile support"),
            data.len() as u64
        );
        assert_eq!(fs::read(&destination).unwrap(), data);
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o755
        );

        assert_eq!(
            clone_file(&source, &destination).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        fs::write(&source, b"source changed").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), data);

        fs::write(&destination, b"clone changed").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"source changed");
    }
}
