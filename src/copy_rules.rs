use crate::git;
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

pub struct CopyRules(Gitignore);

impl CopyRules {
    pub fn load(cwd: &Path, source: &Path, patterns: &[String]) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(source);

        let global = git::optional(cwd, &["config", "--path", "--get", "gwt.ignoreFile"])?;
        match global {
            Some(path) => {
                if let Some(error) = builder.add(source.join(path)) {
                    return Err(error).context("loading gwt.ignoreFile");
                }
            }
            None => {
                if let Some(path) = default_file() {
                    add_if_present(&mut builder, &path)?;
                }
            }
        }

        add_if_present(&mut builder, &source.join(".wtignore"))?;

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

fn default_file() -> Option<PathBuf> {
    let config = match env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(env::var_os("HOME")?).join(".config"),
    };
    Some(config.join("git/wtignore"))
}

fn add_if_present(builder: &mut GitignoreBuilder, path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => match builder.add(path) {
            Some(error) => Err(error).with_context(|| format!("loading {}", path.display())),
            None => Ok(()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
