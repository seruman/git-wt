use crate::git;
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{fs, io, path::Path};

pub struct CopyRules(Gitignore);

impl CopyRules {
    pub fn load(cwd: &Path, source: &Path, patterns: &[String]) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(source);

        let global = git::optional(cwd, &["config", "--path", "--get", "gwt.ignoreFile"])?;
        if let Some(path) = global
            && let Some(error) = builder.add(source.join(path))
        {
            return Err(error).context("loading gwt.ignoreFile");
        }

        let local = source.join(".wtignore");
        match fs::symlink_metadata(&local) {
            Ok(_) => {
                if let Some(error) = builder.add(&local) {
                    return Err(error).context("loading .wtignore");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("reading .wtignore"),
        }

        for pattern in patterns {
            builder
                .add_line(None, pattern)
                .context("parsing --ignore")?;
        }

        Ok(Self(builder.build()?))
    }

    pub fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        path.ancestors()
            .take_while(|p| !p.as_os_str().is_empty())
            .enumerate()
            .any(|(depth, p)| self.0.matched(p, is_dir || depth > 0).is_ignore())
    }
}
