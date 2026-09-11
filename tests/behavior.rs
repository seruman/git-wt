use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn ps(path: &Path) -> &str {
    path.to_str().unwrap()
}

struct Repo {
    _temp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    trees: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let repo = Self {
            _temp: temp,
            home: base.join("home"),
            root: base.join("repo"),
            trees: base.join("trees"),
        };

        fs::create_dir(&repo.root).unwrap();
        fs::create_dir_all(repo.home.join("templates/hooks")).unwrap();
        fs::create_dir_all(repo.home.join("templates/info")).unwrap();
        fs::write(repo.home.join("templates/info/exclude"), "").unwrap();

        repo.git(&repo.root, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            repo.git(&repo.root, &["config", key, value]);
        }

        repo.git(&repo.root, &["config", "gwt.root", ps(&repo.trees)]);
        repo.git(
            &repo.root,
            &[
                "remote",
                "add",
                "origin",
                "git@gitlab.example:group/team/repo.git",
            ],
        );

        fs::write(repo.root.join("tracked"), "base\n").unwrap();
        fs::write(repo.root.join("deleted"), "delete me\n").unwrap();
        fs::write(repo.root.join(".gitignore"), ".env\ncache/\n").unwrap();
        repo.git(&repo.root, &["add", "."]);
        repo.git(&repo.root, &["commit", "-qm", "initial"]);

        repo
    }

    fn command(&self, program: &str) -> Command {
        let mut child = Command::new(program);
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                child.env_remove(name);
            }
        }

        child
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("xdg"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TEMPLATE_DIR", self.home.join("templates"));

        child
    }

    fn git(&self, path: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();

        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        String::from_utf8(out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .into()
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
        self.run_with_env(cwd, args, &[])
    }

    fn run_with_env(
        &self,
        cwd: &Path,
        args: &[&str],
        environment: &[(&str, &str)],
    ) -> anyhow::Result<String> {
        let output = self
            .command(env!("CARGO_BIN_EXE_git-wt"))
            .current_dir(cwd)
            .args(args)
            .envs(environment.iter().copied())
            .output()?;
        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }

        Ok(String::from_utf8(output.stdout)?)
    }

    fn new_tree(&self, args: &[&str]) -> PathBuf {
        let output = self.run(&self.root, args).unwrap();
        PathBuf::from(output.strip_suffix('\n').unwrap())
    }
}

#[test]
fn ignored_entries_removed_after_planning_do_not_abort_creation() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let r = Repo::new();
    fs::write(r.root.join(".env"), "transient environment").unwrap();

    fs::create_dir(r.root.join("cache")).unwrap();
    fs::set_permissions(r.root.join("cache"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(r.root.join("cache/artifact"), "transient build output").unwrap();
    symlink("missing", r.root.join("cache/link")).unwrap();

    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nset -eu\nrm -rf -- \"$GWT_TEST_SOURCE/.env\" \"$GWT_TEST_SOURCE/cache\"\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

    let output = r
        .run_with_env(
            &r.root,
            &["new", "vanished", "--json"],
            &[("GWT_TEST_SOURCE", ps(&r.root))],
        )
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    let tree = PathBuf::from(output["path"].as_str().unwrap());

    assert_eq!(fs::read_to_string(tree.join("tracked")).unwrap(), "base\n");

    assert!(!tree.join(".env").exists());
    assert!(!tree.join("cache/artifact").exists());
    assert!(fs::symlink_metadata(tree.join("cache/link")).is_err());

    assert_eq!(
        fs::metadata(tree.join("cache"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    assert_eq!(output["clone"]["files"], 0);
    assert_eq!(
        r.git(&tree, &["symbolic-ref", "HEAD"]),
        "refs/heads/vanished"
    );
}

#[test]
fn dirty_planning_only_tolerates_disappearing_ignored_contents() {
    use std::os::unix::fs::PermissionsExt;

    for (entry, should_succeed) in [
        (".env", true),
        ("cache/volatile", true),
        ("tracked", false),
        ("untracked", false),
        ("work", false),
        ("cache/protected", false),
    ] {
        let r = Repo::new();
        fs::write(r.root.join(".env"), "environment").unwrap();
        fs::write(r.root.join("untracked"), "working file").unwrap();

        for directory in ["cache/volatile", "cache/protected", "work"] {
            fs::create_dir_all(r.root.join(directory)).unwrap();
            fs::write(r.root.join(directory).join("keep"), "contents").unwrap();
        }

        // An ignored directory can still contain tracked working contents.
        r.git(&r.root, &["add", "-f", "cache/protected/keep"]);
        let source_index = fs::read(r.root.join(".git/index")).unwrap();

        let destination = r.trees.join("vanished-dirty");
        let marker = r.root.parent().unwrap().join("hook-ran");

        // --dirty uses --no-checkout, so exercise the real ref transaction
        // instead of post-checkout. Branch creation happens after planning.
        let hook = r.root.join(".git/hooks/reference-transaction");
        fs::write(&hook, "#!/bin/sh\nset -eu\n[ \"$1\" = committed ] || exit 0\nwhile read -r old new ref; do\n  if [ \"$ref\" = refs/heads/vanished-dirty ]; then\n    rm -rf -- \"$GWT_TEST_SOURCE/$GWT_TEST_ENTRY\"\n    : > \"$GWT_TEST_MARKER\"\n  fi\ndone\n").unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

        let output = r
            .command(env!("CARGO_BIN_EXE_git-wt"))
            .current_dir(&r.root)
            .args([
                "new",
                "vanished-dirty",
                "--dirty",
                "--json",
                "--path",
                ps(&destination),
            ])
            .env("GWT_TEST_SOURCE", &r.root)
            .env("GWT_TEST_ENTRY", entry)
            .env("GWT_TEST_MARKER", &marker)
            .output()
            .unwrap();

        assert!(marker.is_file(), "hook did not exercise {entry}");
        assert!(!r.root.join(entry).exists());

        assert_eq!(
            output.status.success(),
            should_succeed,
            "{entry}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);

        if should_succeed {
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["mode"], "dirty");
            assert!(destination.join(".git").is_file());

            for protected in ["tracked", "untracked", "work/keep", "cache/protected/keep"] {
                assert!(destination.join(protected).is_file(), "{protected}");
            }
        } else {
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert!(!destination.exists());
            assert!(String::from_utf8_lossy(&output.stderr).contains("retained"));
        }

        r.git(
            &r.root,
            &["rev-parse", "--verify", "refs/heads/vanished-dirty"],
        );
    }
}

#[test]
fn cloned_replacement_symlink_never_changes_external_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    let external = r.root.parent().unwrap().join("external");
    fs::write(&external, "private sentinel").unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();

    fs::write(r.root.join(".env"), "planned regular file").unwrap();
    fs::set_permissions(r.root.join(".env"), fs::Permissions::from_mode(0o644)).unwrap();

    let index = fs::read(r.root.join(".git/index")).unwrap();

    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nset -eu\nrm \"$GWT_TEST_SOURCE/.env\"\nln -s \"$GWT_TEST_EXTERNAL\" \"$GWT_TEST_SOURCE/.env\"\n").unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

    let destination = r.trees.join("replacement");
    let result = r.run_with_env(
        &r.root,
        &["new", "replacement", "--json", "--path", ps(&destination)],
        &[
            ("GWT_TEST_SOURCE", ps(&r.root)),
            ("GWT_TEST_EXTERNAL", ps(&external)),
        ],
    );

    assert_eq!(
        fs::metadata(&external).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_to_string(external).unwrap(), "private sentinel");

    let error = result.unwrap_err().to_string();
    assert!(error.contains("regular file"), "{error}");
    assert!(!destination.exists());

    assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read_to_string(r.root.join("tracked")).unwrap(),
        "base\n"
    );

    r.git(
        &r.root,
        &["rev-parse", "--verify", "refs/heads/replacement"],
    );
    assert_eq!(
        r.git(&r.root, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn ignored_root_symlinks_are_leaves_even_when_dangling_or_cyclic() {
    use std::os::unix::fs::symlink;

    let r = Repo::new();
    let external = r.root.parent().unwrap().join("external-directory");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("sentinel"), "outside").unwrap();

    fs::write(r.root.join(".git/info/exclude"), "ignored-*\n").unwrap();
    let links = [
        ("ignored-file", external.join("sentinel")),
        ("ignored-directory", external.clone()),
        ("ignored-dangling", external.join("absent")),
        ("ignored-relative", PathBuf::from("tracked")),
        ("ignored-internal", r.root.join("tracked")),
        ("ignored-cycle", PathBuf::from("ignored-cycle")),
    ];
    for (name, target) in &links {
        symlink(target, r.root.join(name)).unwrap();
    }

    for dirty in [false, true] {
        let branch = if dirty { "links-dirty" } else { "links" };
        let mut args = vec!["new", branch, "--json"];
        if dirty {
            args.push("--dirty");
        }

        let value: Value = serde_json::from_str(&r.run(&r.root, &args).unwrap()).unwrap();
        let destination = Path::new(value["path"].as_str().unwrap());

        assert_eq!(value["clone"]["symlinks"], 6);
        for (name, target) in &links {
            assert!(destination.join(name).is_symlink(), "{name}");
            let expected = if *name == "ignored-internal" {
                destination.join("tracked")
            } else {
                target.clone()
            };
            assert_eq!(fs::read_link(destination.join(name)).unwrap(), expected);
        }

        assert_eq!(
            fs::read_to_string(external.join("sentinel")).unwrap(),
            "outside"
        );
    }

    let excluded = r.new_tree(&["new", "excluded-link", "--exclude", "ignored-directory"]);
    assert!(!excluded.join("ignored-directory").is_symlink());
    assert!(excluded.join("ignored-dangling").is_symlink());
}

#[test]
fn retargeting_preserves_parent_traversal_and_external_link_meaning() {
    use std::os::unix::fs::symlink;

    let r = Repo::new();
    let external = r.root.parent().unwrap().join("shared");
    fs::write(&external, "external sentinel").unwrap();

    fs::create_dir(r.root.join("cache")).unwrap();
    fs::create_dir(r.root.join("sub")).unwrap();

    fs::create_dir_all(r.trees.join("elsewhere/sub")).unwrap();
    fs::write(r.trees.join("elsewhere/shared"), "hop sentinel").unwrap();
    symlink(r.trees.join("elsewhere/sub"), r.root.join("hop")).unwrap();

    let preserved = [
        ("escape", r.root.join("../shared")),
        ("internal-parent", r.root.join("sub/../tracked")),
        ("symlink-parent", r.root.join("hop/../shared")),
        ("dangling-escape", r.root.join("../missing")),
        ("external", external.clone()),
        ("relative", PathBuf::from("../tracked")),
    ];
    for (name, target) in &preserved {
        symlink(target, r.root.join("cache").join(name)).unwrap();
    }

    symlink(
        r.root.join("missing"),
        r.root.join("cache/internal-dangling"),
    )
    .unwrap();

    let alias = r
        .root
        .strip_prefix("/private")
        .map(|relative| Path::new("/").join(relative))
        .unwrap_or_else(|_| r.root.clone());
    symlink(alias.join("tracked"), r.root.join("cache/internal-alias")).unwrap();

    let value: Value =
        serde_json::from_str(&r.run(&r.root, &["new", "retarget", "--json"]).unwrap()).unwrap();
    let destination = Path::new(value["path"].as_str().unwrap());

    assert_eq!(value["clone"]["symlinks"], 8);

    for (name, target) in &preserved {
        assert_eq!(
            fs::read_link(destination.join("cache").join(name)).unwrap(),
            *target
        );
    }

    assert_eq!(
        fs::read_to_string(destination.join("cache/escape")).unwrap(),
        "external sentinel"
    );
    assert_eq!(
        fs::read_to_string(destination.join("cache/symlink-parent")).unwrap(),
        "hop sentinel"
    );

    assert_eq!(
        fs::read_link(destination.join("cache/internal-dangling")).unwrap(),
        destination.join("missing")
    );
    assert_eq!(
        fs::read_link(destination.join("cache/internal-alias")).unwrap(),
        destination.join("tracked")
    );

    assert_eq!(fs::read_to_string(external).unwrap(), "external sentinel");
}

#[test]
fn automatic_branch_names_ignore_same_named_tags() {
    for branch in ["main", "feature/topic"] {
        let r = Repo::new();
        if branch != "main" {
            r.git(&r.root, &["checkout", "-b", branch]);
        }

        r.git(&r.root, &["branch", &format!("{branch}-1")]);
        r.git(&r.root, &["tag", branch]);

        let tree = r.new_tree(&["new", "--no-clone"]);

        assert_eq!(
            r.git(&tree, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{branch}-2")
        );
        assert_eq!(
            tree,
            r.trees
                .join("gitlab.example/group/team/repo")
                .join(format!("{branch}-2"))
        );

        assert_eq!(
            r.git(&r.root, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{branch}")
        );
    }
}

#[test]
fn path_rejects_stale_nested_checkouts_instead_of_discovering_the_parent() {
    let r = Repo::new();
    let nested = r.new_tree(&["new", "nested", "--no-clone", "--path", "nested"]);
    fs::remove_file(nested.join(".git")).unwrap();
    let index = fs::read(r.root.join(".git/index")).unwrap();

    for json in [false, true] {
        let mut child = r.command(env!("CARGO_BIN_EXE_git-wt"));
        child.current_dir(&r.root).args(["path", "nested"]);
        if json {
            child.arg("--json");
        }
        let output = child.output().unwrap();

        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("accessible working tree"));
    }

    assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), index);
    assert_eq!(r.git(&r.root, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert!(nested.join("tracked").is_file());
}

#[test]
fn path_rejects_an_unrelated_repository_at_a_registered_location() {
    let r = Repo::new();
    let tree = r.new_tree(&["new", "replaced", "--no-clone"]);
    fs::remove_file(tree.join(".git")).unwrap();
    r.git(&tree, &["init", "-q", "-b", "foreign"]);
    fs::write(tree.join("sentinel"), "foreign repository").unwrap();
    let config = fs::read(tree.join(".git/config")).unwrap();

    for json in [false, true] {
        let mut child = r.command(env!("CARGO_BIN_EXE_git-wt"));
        child.current_dir(&r.root).args(["path", "replaced"]);
        if json {
            child.arg("--json");
        }
        let output = child.output().unwrap();

        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("accessible working tree"));
    }

    assert_eq!(fs::read(tree.join(".git/config")).unwrap(), config);
    assert_eq!(
        fs::read_to_string(tree.join("sentinel")).unwrap(),
        "foreign repository"
    );
    assert_eq!(
        r.git(&tree, &["symbolic-ref", "HEAD"]),
        "refs/heads/foreign"
    );
}

#[test]
fn symlinked_repository_parents_work_without_allowing_child_escapes() {
    use std::os::unix::fs::symlink;

    let r = Repo::new();
    let parent = r.trees.join("gitlab.example/group/team/repo");
    let actual = r.root.parent().unwrap().join("actual");
    let outside = r.root.parent().unwrap().join("outside");

    fs::create_dir(&actual).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "outside").unwrap();

    fs::create_dir_all(parent.parent().unwrap()).unwrap();
    symlink(&actual, &parent).unwrap();

    let root_alias = r
        .trees
        .strip_prefix("/private")
        .map(|relative| Path::new("/").join(relative))
        .unwrap_or_else(|_| r.trees.clone());
    r.git(&r.root, &["config", "gwt.root", ps(&root_alias)]);

    fs::write(r.root.join(".env"), "environment").unwrap();

    for no_clone in [false, true] {
        let branch = if no_clone { "ordinary" } else { "cloned" };
        let mut args = vec!["new", branch];
        if no_clone {
            args.push("--no-clone");
        }

        let tree = r.new_tree(&args);

        assert_eq!(tree, actual.join(branch));
        assert_eq!(tree.join(".env").exists(), !no_clone);
        assert!(tree.join(".git").is_file());
    }

    symlink(&outside, actual.join("escape")).unwrap();
    let error = r
        .run(&r.root, &["new", "escape/topic", "--no-clone"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("escape"), "{error}");
    assert!(!outside.join("topic").exists());

    symlink(actual.join("cloned"), actual.join("occupied")).unwrap();
    symlink(actual.join("absent"), actual.join("dangling")).unwrap();
    for branch in ["occupied", "dangling"] {
        assert!(r.run(&r.root, &["new", branch, "--no-clone"]).is_err());
        assert!(actual.join(branch).is_symlink());
    }

    assert_eq!(
        fs::read_to_string(outside.join("sentinel")).unwrap(),
        "outside"
    );
    assert!(actual.join("cloned/.git").is_file());
    assert!(actual.join("ordinary/.git").is_file());
}

#[test]
fn apfs_population_preserves_read_only_file_and_directory_modes() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    let cache = r.root.join("cache");
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o750)).unwrap();
    for (name, mode) in [("read-only", 0o444), ("executable", 0o555)] {
        fs::write(cache.join(name), "contents").unwrap();
        fs::set_permissions(cache.join(name), fs::Permissions::from_mode(mode)).unwrap();
    }

    for dirty in [false, true] {
        let branch = if dirty { "modes-dirty" } else { "modes" };
        let mut args = vec!["new", branch];
        if dirty {
            args.push("--dirty");
        }

        let destination = r.new_tree(&args);

        for (name, mode) in [
            ("cache", 0o750),
            ("cache/read-only", 0o444),
            ("cache/executable", 0o555),
        ] {
            assert_eq!(
                fs::metadata(destination.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                mode
            );
        }

        assert_eq!(
            fs::read_to_string(destination.join("cache/read-only")).unwrap(),
            "contents"
        );
    }
}

#[test]
fn stdout_errors_other_than_broken_pipe_fail_without_undoing_creation() {
    use std::{
        os::{fd::OwnedFd, unix::net::UnixDatagram},
        process::Stdio,
    };

    let r = Repo::new();
    fs::write(r.root.join(".env"), "environment").unwrap();
    let destination = r.trees.join("unwritable-stdout");

    let output = r
        .command(env!("CARGO_BIN_EXE_git-wt"))
        .current_dir(&r.root)
        .args(["new", "unwritable-stdout", "--path", ps(&destination)])
        // An unconnected datagram socket reports a real non-BrokenPipe write
        // error. Rust's standard stdout silently accepts EBADF, so a read-only
        // file descriptor does not exercise this error branch.
        .stdout(Stdio::from(OwnedFd::from(UnixDatagram::unbound().unwrap())))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("writing stdout"), "{error}");
    assert!(!error.contains("panicked"), "{error}");

    assert!(destination.join(".git").is_file());
    assert_eq!(
        fs::read_to_string(destination.join(".env")).unwrap(),
        "environment"
    );

    assert_eq!(
        fs::read_to_string(r.root.join("tracked")).unwrap(),
        "base\n"
    );
}

#[test]
fn branch_selectors_do_not_require_a_traversable_source_relative_path() {
    let r = Repo::new();
    let destination = r.new_tree(&["new", "tracked/topic", "--no-clone"]);

    assert_eq!(
        r.run(&r.root, &["path", "tracked/topic"]).unwrap(),
        format!("{}\n", destination.display())
    );

    r.run(&r.root, &["remove", "tracked/topic"]).unwrap();
    assert!(!destination.exists());
    assert_eq!(
        fs::read_to_string(r.root.join("tracked")).unwrap(),
        "base\n"
    );

    let branch = r.new_tree(&["new", "ambiguous", "--no-clone"]);
    let nested = r.new_tree(&[
        "new",
        "different-branch",
        "--no-clone",
        "--path",
        "ambiguous",
    ]);

    assert!(
        r.run(&r.root, &["path", "ambiguous"])
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    assert_eq!(
        r.run(&r.root, &["path", ps(&branch)]).unwrap(),
        format!("{}\n", branch.display())
    );
    assert!(nested.join(".git").is_file());
}

#[test]
fn ignored_environment_and_real_git_worktree() {
    let r = Repo::new();
    fs::write(r.root.join(".env"), "secret").unwrap();
    fs::create_dir(r.root.join("cache")).unwrap();
    fs::write(r.root.join("cache/data"), [0, 1, 255, 4]).unwrap();

    fs::write(r.root.join("tracked"), "dirty").unwrap();
    fs::write(r.root.join("untracked"), "not ignored").unwrap();

    let tree = r.new_tree(&["new", "fix"]);

    assert_eq!(tree, r.trees.join("gitlab.example/group/team/repo/fix"));
    assert!(tree.join(".git").is_file());
    assert_eq!(
        r.git(&tree, &["rev-parse", "--git-common-dir"]),
        ps(&r.root.join(".git"))
    );

    assert_ne!(
        r.git(&tree, &["rev-parse", "--git-path", "index"]),
        r.git(&r.root, &["rev-parse", "--git-path", "index"])
    );

    assert_eq!(fs::read_to_string(tree.join("tracked")).unwrap(), "base\n");
    assert!(!tree.join("untracked").exists());
    assert_eq!(fs::read_to_string(tree.join(".env")).unwrap(), "secret");

    fs::write(tree.join("cache/data"), "changed").unwrap();
    assert_eq!(fs::read(r.root.join("cache/data")).unwrap(), [0, 1, 255, 4]);

    fs::write(tree.join("tracked"), "new commit").unwrap();
    r.git(&tree, &["add", "tracked"]);
    r.git(&tree, &["commit", "-qm", "work"]);
    assert_ne!(
        r.git(&tree, &["rev-parse", "HEAD"]),
        r.git(&r.root, &["rev-parse", "HEAD"])
    );
}

#[test]
fn dirty_contents_deletions_staging_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let r = Repo::new();
    symlink(r.root.join("tracked"), r.root.join("tracked-link")).unwrap();
    r.git(&r.root, &["add", "tracked-link"]);
    r.git(&r.root, &["commit", "-qm", "link"]);

    fs::write(r.root.join("tracked"), "staged").unwrap();
    r.git(&r.root, &["add", "tracked"]);

    fs::write(r.root.join("tracked"), "working").unwrap();
    fs::set_permissions(r.root.join("tracked"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(r.root.join("deleted")).unwrap();
    fs::write(r.root.join("untracked"), [0, 255, 0]).unwrap();
    fs::write(r.root.join(".env"), "env").unwrap();

    symlink("tracked", r.root.join("relative-link")).unwrap();
    symlink("/outside/path", r.root.join("external-link")).unwrap();

    let source_index = fs::read(r.root.join(".git/index")).unwrap();

    let tree = r.new_tree(&["new", "dirty", "--dirty"]);

    assert_eq!(fs::read_to_string(tree.join("tracked")).unwrap(), "working");
    assert_eq!(
        fs::metadata(tree.join("tracked"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(!tree.join("deleted").exists());
    assert_eq!(fs::read(tree.join("untracked")).unwrap(), [0, 255, 0]);

    assert_eq!(
        fs::read_link(tree.join("tracked-link")).unwrap(),
        tree.join("tracked")
    );
    assert_eq!(
        fs::read_link(tree.join("relative-link")).unwrap(),
        Path::new("tracked")
    );
    assert_eq!(
        fs::read_link(tree.join("external-link")).unwrap(),
        Path::new("/outside/path")
    );

    assert!(
        r.git(&tree, &["diff", "--cached", "--name-only"])
            .is_empty()
    );
    assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
}

#[test]
fn source_environment_is_honored_without_redirecting_destination_git() {
    let r = Repo::new();
    let subdirectory = r.root.join("subdirectory");
    fs::create_dir(&subdirectory).unwrap();

    fs::write(r.root.join("tracked"), "staged").unwrap();
    r.git(&r.root, &["add", "tracked"]);

    fs::write(r.root.join("tracked"), "working").unwrap();
    fs::write(r.root.join(".env"), "excluded by command-line config").unwrap();

    let source_index_path = r.root.join(".git/index");
    let source_index = fs::read(&source_index_path).unwrap();
    let destination = r.root.parent().unwrap().join("context-tree");

    let output = r
        .run_with_env(
            &subdirectory,
            &["new", "context", "--dirty", "--path", "../../context-tree"],
            &[
                ("GIT_DIR", "../.git"),
                ("GIT_WORK_TREE", ".."),
                ("GIT_INDEX_FILE", ps(&source_index_path)),
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "gwt.exclude"),
                ("GIT_CONFIG_VALUE_0", ".env"),
            ],
        )
        .unwrap();

    assert_eq!(Path::new(output.trim_end()), destination);
    assert_eq!(
        fs::read_to_string(destination.join("tracked")).unwrap(),
        "working"
    );
    assert!(!destination.join(".env").exists());

    assert!(
        r.git(&destination, &["diff", "--cached", "--name-only"])
            .is_empty()
    );

    assert_eq!(fs::read(&source_index_path).unwrap(), source_index);
    assert_eq!(
        fs::read_to_string(r.root.join("tracked")).unwrap(),
        "working"
    );
}

#[test]
fn worktree_creation_does_not_use_the_sources_index() {
    for no_clone in [false, true] {
        let r = Repo::new();

        // Select a linked source via relative Git selectors from a subdirectory.
        let source = r.new_tree(&["new", "source", "--no-clone"]);
        let subdirectory = source.join("subdirectory");
        fs::create_dir(&subdirectory).unwrap();

        fs::write(source.join("tracked"), "staged").unwrap();
        r.git(&source, &["add", "tracked"]);

        fs::write(source.join("tracked"), "working").unwrap();
        fs::write(source.join(".env"), "environment").unwrap();

        let index = PathBuf::from(r.git(
            &source,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        ));
        let before = fs::read(&index).unwrap();

        let destination = r.trees.join("index-isolated");
        let mut args = vec!["new", "topic", "--path", ps(&destination)];
        if no_clone {
            args.push("--no-clone");
        }

        let output = r
            .run_with_env(
                &subdirectory,
                &args,
                &[
                    ("GIT_DIR", "../.git"),
                    ("GIT_WORK_TREE", ".."),
                    ("GIT_INDEX_FILE", ps(&index)),
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "gwt.exclude"),
                    ("GIT_CONFIG_VALUE_0", "cache"),
                ],
            )
            .unwrap();

        assert_eq!(output, format!("{}\n", destination.display()));

        assert_eq!(fs::read(&index).unwrap(), before);
        assert_eq!(r.git(&source, &["show", ":tracked"]), "staged");
        assert_eq!(
            fs::read_to_string(source.join("tracked")).unwrap(),
            "working"
        );

        assert_eq!(r.git(&destination, &["show", ":tracked"]), "base");
        assert!(r.git(&destination, &["status", "--porcelain"]).is_empty());
        assert_eq!(destination.join(".env").exists(), !no_clone);
    }
}

#[test]
fn removal_checks_the_destinations_index_even_with_a_source_override() {
    let r = Repo::new();
    let destination = r.new_tree(&["new", "staged-only", "--no-clone"]);
    fs::write(destination.join("tracked"), "staged-only contents").unwrap();
    r.git(&destination, &["add", "tracked"]);
    fs::write(destination.join("tracked"), "base\n").unwrap();

    let before = fs::read(r.root.join(".git/index")).unwrap();

    for index in [r.root.join(".git/index"), PathBuf::from(".git/index")] {
        let output = r
            .command(env!("CARGO_BIN_EXE_git-wt"))
            .current_dir(&r.root)
            .args(["remove", "staged-only"])
            .env("GIT_INDEX_FILE", index)
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());

        assert!(destination.join(".git").is_file());
        assert_eq!(
            r.git(&destination, &["show", ":tracked"]),
            "staged-only contents"
        );

        assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), before);
    }
}

#[test]
fn failed_add_preserves_source_staging_and_git_command_line_config() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    fs::write(r.root.join("tracked"), "staged").unwrap();
    r.git(&r.root, &["add", "tracked"]);
    let source_index = r.root.join(".git/index");
    let before = fs::read(&source_index).unwrap();

    let hooks = r.root.parent().unwrap().join("custom-hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'configured hook failed' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

    let binary_dir = Path::new(env!("CARGO_BIN_EXE_git-wt")).parent().unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(binary_dir.to_path_buf()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    let destination = r.trees.join("hook-index");
    let output = r
        .command("git")
        .args([
            "-C",
            ps(&r.root),
            "-c",
            &format!("core.hooksPath={}", hooks.display()),
            "wt",
            "new",
            "hook-index",
            "--path",
            ps(&destination),
        ])
        .env("PATH", path)
        .env("GIT_INDEX_FILE", &source_index)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("configured hook failed"), "{error}");
    assert!(error.contains("retained"), "{error}");

    assert!(!destination.exists());

    assert_eq!(fs::read(source_index).unwrap(), before);
    assert_eq!(r.git(&r.root, &["show", ":tracked"]), "staged");

    assert_eq!(
        r.git(&r.root, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn clone_source_ignored_even_when_target_does_not_ignore() {
    let r = Repo::new();
    r.git(&r.root, &["checkout", "-qb", "other"]);
    fs::write(r.root.join(".gitignore"), "cache/\n").unwrap();
    r.git(&r.root, &["commit", "-qam", "different ignore rules"]);

    r.git(&r.root, &["checkout", "-q", "main"]);
    fs::write(r.root.join(".env"), "local environment").unwrap();
    let before = fs::read(r.root.join(".git/info/exclude")).unwrap();

    let tree = r.new_tree(&["new", "other"]);

    assert_eq!(
        fs::read_to_string(tree.join(".env")).unwrap(),
        "local environment"
    );
    assert!(r.git(&tree, &["status", "--porcelain"]).contains("?? .env"));

    assert_eq!(fs::read(r.root.join(".git/info/exclude")).unwrap(), before);
}

#[test]
fn target_checkout_and_hook_files_win_including_symlink_parents() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    r.git(&r.root, &["checkout", "-qb", "other"]);
    fs::write(r.root.join(".env"), "tracked target").unwrap();
    r.git(&r.root, &["add", "-f", ".env"]);
    r.git(&r.root, &["commit", "-qm", "target config"]);

    r.git(&r.root, &["checkout", "-q", "main"]);
    fs::write(r.root.join(".env"), "source ignored").unwrap();
    fs::create_dir(r.root.join("cache")).unwrap();
    fs::write(r.root.join("cache/data"), "source").unwrap();

    let outside = r.trees.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!("#!/bin/sh\nln -s '{}' cache\n", outside.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let tree = r.new_tree(&["new", "other"]);

    assert_eq!(
        fs::read_to_string(tree.join(".env")).unwrap(),
        "tracked target"
    );

    assert!(tree.join("cache").is_symlink());
    assert!(!outside.join("data").exists());
}

#[test]
fn unnamed_creation_creates_distinct_branches_and_skips_occupied_names() {
    let r = Repo::new();
    r.git(&r.root, &["branch", "main-1"]);

    let occupied = r.trees.join("gitlab.example/group/team/repo/main-2");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("keep"), "keep").unwrap();

    let a = r.new_tree(&["new", "--no-clone"]);
    let b = r.new_tree(&["new", "--no-clone"]);

    assert_eq!(a.file_name().unwrap(), "main-3");
    assert_eq!(b.file_name().unwrap(), "main-4");
    assert_eq!(r.git(&a, &["symbolic-ref", "--short", "HEAD"]), "main-3");

    assert_eq!(
        r.git(&a, &["rev-parse", "HEAD"]),
        r.git(&r.root, &["rev-parse", "HEAD"])
    );

    assert!(occupied.join("keep").exists());

    assert!(r.run(&r.root, &["new", "main", "--no-clone"]).is_err());
}

#[test]
fn detached_source_uses_a_new_named_branch() {
    let r = Repo::new();
    r.git(&r.root, &["checkout", "--detach", "-q"]);

    let tree = r.new_tree(&["new", "--no-clone"]);

    assert_eq!(
        r.git(&tree, &["symbolic-ref", "--short", "HEAD"]),
        "detached-1"
    );
}

#[test]
fn source_is_current_linked_worktree_and_paths_survive_git_moves() {
    let r = Repo::new();
    let tree = r.new_tree(&["new", "feature", "--no-clone"]);
    fs::write(tree.join(".env"), "linked env").unwrap();

    fs::write(tree.join("tracked"), "linked commit").unwrap();
    r.git(&tree, &["commit", "-qam", "linked"]);

    let moved = r.trees.join("moved");
    r.git(&r.root, &["worktree", "move", ps(&tree), ps(&moved)]);

    let child = PathBuf::from(r.run(&moved, &["new"]).unwrap().trim_end_matches('\n'));

    assert_eq!(
        fs::read_to_string(child.join(".env")).unwrap(),
        "linked env"
    );

    assert_eq!(
        r.git(&child, &["rev-parse", "HEAD"]),
        r.git(&moved, &["rev-parse", "HEAD"])
    );

    assert_eq!(
        r.run(&r.root, &["path", "feature"]).unwrap(),
        format!("{}\n", moved.display())
    );

    assert_eq!(
        r.run(&child, &["path", "--main-worktree"]).unwrap(),
        format!("{}\n", r.root.display())
    );

    r.git(&moved, &["branch", "-m", "renamed"]);
    assert!(r.run(&r.root, &["path", "renamed"]).is_ok());
}

#[test]
fn exclusions_only_remove_ignored_files_and_match_components() {
    let r = Repo::new();
    fs::create_dir(r.root.join("cache")).unwrap();
    fs::write(r.root.join("cache/tracked"), "tracked cache").unwrap();
    r.git(&r.root, &["add", "-f", "cache/tracked"]);
    r.git(&r.root, &["commit", "-qm", "tracked cache"]);

    fs::write(r.root.join("cache/ignored"), "drop").unwrap();
    fs::write(r.root.join(".env"), "drop").unwrap();
    fs::write(r.root.join("target"), "nonignored").unwrap();
    fs::write(r.root.join("target-old"), "keep").unwrap();

    r.git(&r.root, &["config", "--add", "gwt.exclude", ".env"]);

    let tree = r.new_tree(&[
        "new",
        "dirty",
        "--dirty",
        "--exclude",
        "./cache/",
        "--exclude",
        "target",
    ]);

    assert!(tree.join("cache/tracked").is_file());
    assert!(!tree.join("cache/ignored").exists());

    assert!(tree.join("target").is_file());
    assert!(tree.join("target-old").is_file());

    assert!(!tree.join(".env").exists());
}

#[test]
fn symlink_parent_selectors_remove_only_the_physically_named_worktree() {
    use std::os::unix::fs::symlink;

    let r = Repo::new();
    let wrong = r.root.join("victim");
    let intended = r.trees.join("victim");
    fs::create_dir_all(r.trees.join("sub")).unwrap();
    r.git(&r.root, &["worktree", "add", "-b", "wrong", ps(&wrong)]);
    r.git(
        &r.root,
        &["worktree", "add", "-b", "intended", ps(&intended)],
    );
    fs::write(wrong.join(".env"), "must survive").unwrap();

    symlink(r.trees.join("sub"), r.root.join("link")).unwrap();
    assert_eq!(
        r.run(&r.root, &["path", "link/../victim"]).unwrap(),
        format!("{}\n", intended.display())
    );

    r.run(&r.root, &["remove", "link/../victim", "--force"])
        .unwrap();
    assert!(!intended.exists());

    assert_eq!(
        fs::read_to_string(wrong.join(".env")).unwrap(),
        "must survive"
    );
    assert_eq!(r.git(&wrong, &["symbolic-ref", "--short", "HEAD"]), "wrong");
}

#[test]
fn symlink_path_creation_preserves_physical_meaning_and_final_collisions() {
    use std::os::unix::fs::symlink;

    let r = Repo::new();
    fs::create_dir_all(r.trees.join("sub")).unwrap();
    symlink(r.trees.join("sub"), r.root.join("link")).unwrap();

    let created = r.new_tree(&[
        "new",
        "created",
        "--no-clone",
        "--path",
        "link/../missing/created",
    ]);

    assert_eq!(created, r.trees.join("missing/created"));
    assert!(!r.root.join("missing").exists());

    symlink(&created, r.root.join("alias")).unwrap();
    assert_eq!(
        r.run(&r.root, &["path", "alias"]).unwrap(),
        format!("{}\n", created.display())
    );

    assert!(
        r.run(
            &r.root,
            &["new", "occupied", "--no-clone", "--path", "alias"]
        )
        .is_err()
    );

    symlink(r.trees.join("absent"), r.root.join("dangling")).unwrap();
    assert!(
        r.run(
            &r.root,
            &["new", "dangling", "--no-clone", "--path", "dangling"]
        )
        .is_err()
    );
    assert!(r.root.join("dangling").is_symlink());

    assert!(
        r.run(
            &r.root,
            &[
                "new",
                "ambiguous",
                "--no-clone",
                "--path",
                "link/../absent/../ambiguous"
            ]
        )
        .is_err()
    );
    assert!(!r.trees.join("ambiguous").exists());
    assert!(!r.root.join("ambiguous").exists());

    assert!(
        r.run(
            &r.root,
            &[
                "new",
                "not-directory",
                "--no-clone",
                "--path",
                "tracked/child"
            ]
        )
        .is_err()
    );

    assert!(created.join(".git").is_file());
}

#[test]
fn explicit_path_bypasses_remote_and_handles_whitespace_json() {
    let r = Repo::new();
    r.git(&r.root, &["remote", "remove", "origin"]);
    let destination = r.trees.join("space tab\tnewline\n");

    let output = r
        .run(
            &r.root,
            &[
                "new",
                "topic/slash",
                "--path",
                ps(&destination),
                "--json",
                "--no-clone",
            ],
        )
        .unwrap();
    let object: Value = serde_json::from_str(&output).unwrap();

    assert_eq!(object["path"], ps(&destination));
    assert_eq!(object["mode"], "none");

    let listing: Value = serde_json::from_str(&r.run(&r.root, &["ls", "--json"]).unwrap()).unwrap();
    assert!(
        listing
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["path"] == ps(&destination) && t["bare"] == false)
    );

    assert_eq!(
        r.run(&r.root, &["path", "topic/slash", "--json"]).unwrap(),
        format!("{}\n", serde_json::json!({"path": destination}))
    );
}

#[test]
fn human_list_has_aligned_path_head_checkout_and_status_columns() {
    let r = Repo::new();
    let tree = r.new_tree(&["new", "feature", "--no-clone"]);
    r.git(&r.root, &["worktree", "lock", ps(&tree)]);

    let detached = r.trees.join("detached");
    r.git(&r.root, &["worktree", "add", "--detach", ps(&detached)]);

    let head = r.git(&r.root, &["rev-parse", "--short=8", "HEAD"]);
    let root = ps(&r.root);
    let tree = ps(&tree);
    let detached = ps(&detached);

    let path_width = [root, tree, detached]
        .into_iter()
        .map(|path| path.chars().count())
        .max()
        .unwrap();

    let kind_width = "detached".chars().count();
    let branch_width = "feature".chars().count();

    assert_eq!(
        r.run(&r.root, &["list"]).unwrap(),
        format!(
            "* {root:<path_width$} {head} {branch_kind:<kind_width$} {main:<branch_width$} -\n  {detached:<path_width$} {head} detached {none:<branch_width$} -\n  {tree:<path_width$} {head} {branch_kind:<kind_width$} feature locked\n",
            branch_kind = "branch",
            main = "main",
            none = "-",
        )
    );
}

#[test]
fn config_url_matching_and_full_namespace() {
    let r = Repo::new();
    let scoped = r.trees.join("company");
    r.git(
        &r.root,
        &["config", "gwt.ssh://git@gitlab.example.root", ps(&scoped)],
    );

    let tree = r.new_tree(&["new", "feature/login", "--no-clone"]);

    assert_eq!(
        tree,
        scoped.join("gitlab.example/group/team/repo/feature/login")
    );
}

#[test]
fn rollback_retains_a_new_branch_for_retry_and_never_reuses_a_path() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    fs::write(r.root.join(".env"), "env").unwrap();

    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf 'hook failed\\n' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let failure = r.run(&r.root, &["new", "retry"]).unwrap_err();

    assert!(failure.to_string().contains("hook failed"));
    assert!(failure.to_string().contains("retained"));

    assert!(
        r.git(&r.root, &["branch", "--list", "retry"])
            .contains("retry")
    );
    assert!(
        !r.trees
            .join("gitlab.example/group/team/repo/retry")
            .exists()
    );

    fs::remove_file(hook).unwrap();
    let tree = r.new_tree(&["new", "retry", "--dirty"]);
    assert!(tree.join(".env").is_file());

    let existing = r.trees.join("occupied");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), "keep").unwrap();

    assert!(
        r.run(&r.root, &["new", "another", "--path", ps(&existing)])
            .is_err()
    );
    assert_eq!(fs::read_to_string(existing.join("keep")).unwrap(), "keep");
}

#[test]
fn remove_leaves_branch_and_delegates_dirty_and_lock_refusals() {
    let r = Repo::new();
    let tree = r.new_tree(&["new", "fix", "--no-clone"]);
    fs::write(tree.join("tracked"), "dirty").unwrap();
    assert!(r.run(&r.root, &["remove", "fix"]).is_err());

    r.git(&r.root, &["worktree", "lock", ps(&tree)]);
    assert!(r.run(&r.root, &["remove", "fix", "--force"]).is_err());

    r.git(&r.root, &["worktree", "unlock", ps(&tree)]);
    r.run(&r.root, &["rm", "fix", "--force"]).unwrap();
    assert!(!tree.exists());
    assert!(r.git(&r.root, &["branch", "--list", "fix"]).contains("fix"));

    assert!(r.run(&r.root, &["remove", "main", "--force"]).is_err());
}

#[test]
fn dirty_rejects_other_base_and_nested_repositories_before_creation() {
    let r = Repo::new();
    let initial = r.git(&r.root, &["rev-parse", "HEAD"]);
    fs::write(r.root.join("tracked"), "new").unwrap();
    r.git(&r.root, &["commit", "-qam", "next"]);

    assert!(
        r.run(&r.root, &["new", "bad", "--dirty", "--base", &initial],)
            .is_err()
    );

    fs::create_dir(r.root.join("nested")).unwrap();
    r.git(&r.root.join("nested"), &["init", "-q"]);
    assert!(r.run(&r.root, &["new", "nested-copy", "--dirty"]).is_err());

    assert!(
        r.git(&r.root, &["branch", "--list", "bad", "nested-copy"])
            .is_empty()
    );
}

#[test]
fn registered_nested_worktrees_are_pruned_during_clone() {
    let r = Repo::new();
    let nested = r.root.join("cache/nested");
    r.git(&r.root, &["worktree", "add", "-b", "nested", ps(&nested)]);
    fs::write(r.root.join("cache/plain"), "keep").unwrap();

    let tree = r.new_tree(&["new", "copy", "--dirty"]);

    assert!(tree.join("cache/plain").exists());
    assert!(!tree.join("cache/nested").exists());
}

#[test]
fn independent_creations_can_run_concurrently() {
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    let r = Repo::new();
    fs::write(r.root.join(".env"), "env").unwrap();
    let source_index = fs::read(r.root.join(".git/index")).unwrap();

    let gate = r.root.parent().unwrap().join("gate");
    fs::create_dir(&gate).unwrap();
    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nset -eu\nbranch=$(git symbolic-ref --short HEAD)\n: > \"$GWT_TEST_GATE/$branch\"\ni=0\nwhile [ ! -f \"$GWT_TEST_GATE/release\" ]; do\n  [ \"$i\" -lt 200 ] || { echo 'hook rendezvous timed out' >&2; exit 1; }\n  sleep 0.05\n  i=$((i + 1))\ndone\n").unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

    let children: Vec<_> = ["parallel-a", "parallel-b"]
        .into_iter()
        .map(|branch| {
            TestChild::spawn(
                r.command(env!("CARGO_BIN_EXE_git-wt"))
                    .current_dir(&r.root)
                    .args(["new", branch])
                    .env("GWT_TEST_GATE", &gate),
            )
        })
        .collect();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !(gate.join("parallel-a").exists() && gate.join("parallel-b").exists()) {
        assert!(
            Instant::now() < deadline,
            "both creations must reach their hooks before either is released"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // A serialized implementation cannot get here: both real Git additions are
    // inside post-checkout, and neither invocation can have finished population.
    fs::write(gate.join("release"), "go").unwrap();

    let mut indexes = Vec::new();
    for child in children {
        let output = child.wait();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let tree = PathBuf::from(
            String::from_utf8(output.stdout)
                .unwrap()
                .strip_suffix('\n')
                .unwrap(),
        );
        assert_eq!(fs::read_to_string(tree.join(".env")).unwrap(), "env");

        indexes.push(r.git(&tree, &["rev-parse", "--git-path", "index"]));
    }

    assert_ne!(indexes[0], indexes[1]);
    assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
}

#[test]
fn concurrent_destination_collision_leaves_the_winner_intact() {
    let r = Repo::new();
    fs::write(r.root.join(".env"), "env").unwrap();
    let destination = r.trees.join("collision");
    let barrier = std::sync::Barrier::new(3);

    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            r.run(&r.root, &["new", "collision-a", "--path", ps(&destination)])
        });

        let b = scope.spawn(|| {
            barrier.wait();
            r.run(&r.root, &["new", "collision-b", "--path", ps(&destination)])
        });

        barrier.wait();
        [a.join().unwrap(), b.join().unwrap()]
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

    assert!(destination.join(".git").is_file());
    assert_eq!(fs::read_to_string(destination.join(".env")).unwrap(), "env");

    let branch = r.git(&destination, &["symbolic-ref", "--short", "HEAD"]);
    assert!(branch == "collision-a" || branch == "collision-b");
    assert_eq!(
        r.git(&r.root, &["worktree", "list", "--porcelain"])
            .matches(&format!("worktree {}", destination.display()))
            .count(),
        1
    );
}

#[test]
fn concurrent_branch_collision_cleans_only_the_loser() {
    let r = Repo::new();
    fs::write(r.root.join(".env"), "env").unwrap();
    let destinations = [r.trees.join("branch-a"), r.trees.join("branch-b")];
    let barrier = std::sync::Barrier::new(3);

    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            r.run(
                &r.root,
                &["new", "shared-branch", "--path", ps(&destinations[0])],
            )
        });

        let b = scope.spawn(|| {
            barrier.wait();
            r.run(
                &r.root,
                &["new", "shared-branch", "--path", ps(&destinations[1])],
            )
        });

        barrier.wait();
        [a.join().unwrap(), b.join().unwrap()]
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

    let winner = PathBuf::from(
        results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .unwrap()
            .trim_end(),
    );
    assert!(winner.join(".git").is_file());
    assert_eq!(fs::read_to_string(winner.join(".env")).unwrap(), "env");

    for destination in destinations {
        assert_eq!(destination.exists(), destination == winner);
    }

    assert_eq!(
        r.git(&r.root, &["worktree", "list", "--porcelain"])
            .matches("branch refs/heads/shared-branch")
            .count(),
        1
    );
}

#[test]
fn cli_errors_and_stdout_contract() {
    let r = Repo::new();
    let binary = env!("CARGO_BIN_EXE_git-wt");

    for args in [
        vec!["new", "--dirty", "--no-clone"],
        vec!["push"],
        vec!["path", "main", "--main-worktree"],
        vec!["path", "--main"],
        vec!["path", "--primary"],
        vec!["-C", ps(&r.root), "list"],
    ] {
        let out = r
            .command(binary)
            .current_dir(&r.root)
            .args(&args)
            .output()
            .unwrap();

        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
    }

    for argument in ["-h", "--help"] {
        let out = r.command(binary).arg(argument).output().unwrap();

        assert!(out.status.success());
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .contains("Usage: git-wt")
        );
    }

    let out = r
        .command(binary)
        .current_dir(&r.root)
        .args(["new", "--no-clone", "--json"])
        .output()
        .unwrap();

    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["branch"], "main-1");
}

#[test]
fn main_worktree_path_is_independent_of_branch_name() {
    let r = Repo::new();
    r.git(&r.root, &["branch", "-m", "trunk"]);
    let linked = r.new_tree(&["new", "main", "--no-clone"]);

    assert_eq!(
        r.run(&linked, &["path", "main"]).unwrap(),
        format!("{}\n", linked.display())
    );
    assert_eq!(
        r.run(&linked, &["path", "--main-worktree"]).unwrap(),
        format!("{}\n", r.root.display())
    );

    let output = r
        .run(&linked, &["path", "--main-worktree", "--json"])
        .unwrap();
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["path"], ps(&r.root));
}

#[test]
fn branch_namespace_collision_and_separate_git_directory() {
    let r = Repo::new();
    r.git(&r.root, &["branch", "main-1/child"]);

    let new = r.new_tree(&["new", "--no-clone"]);
    assert_eq!(new.file_name().unwrap(), "main-2");

    // Move administrative files with Git itself, then exercise discovery from a link.
    let admin = r.trees.join("admin");
    r.git(&r.root, &["init", "--separate-git-dir", ps(&admin)]);
    r.git(&r.root, &["worktree", "repair"]);

    assert_eq!(
        r.run(&r.root, &["path", "--main-worktree"]).unwrap(),
        format!("{}\n", r.root.display())
    );

    // Git itself reports the admin directory from a linked worktree here.
    // Do not invent a registry or return that administrative directory as a checkout.
    assert!(r.run(&new, &["path", "--main-worktree"]).is_err());

    let child = r.run(&new, &["new", "--no-clone"]).unwrap();
    assert!(
        Path::new(child.trim_end_matches('\n'))
            .join(".git")
            .is_file()
    );
}

// A private process group lets timeout/panic cleanup stop Git and its hooks
// before a test drops their temporary directories.
struct TestChild(Option<std::process::Child>, std::time::Instant);

impl TestChild {
    fn spawn(command: &mut Command) -> Self {
        use std::{os::unix::process::CommandExt, process::Stdio};

        Self(
            Some(
                command
                    .process_group(0)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            ),
            std::time::Instant::now() + std::time::Duration::from_secs(15),
        )
    }

    fn signal_process(&self, signal: libc::c_int) {
        let child = self.0.as_ref().unwrap();
        // SAFETY: the child is live and unreaped, so its PID cannot be reused.
        unsafe {
            libc::kill(child.id() as libc::pid_t, signal);
        }
    }

    fn read_stderr_byte(&mut self) -> u8 {
        use std::{io::Read, os::fd::AsRawFd};

        let stderr = self.0.as_mut().unwrap().stderr.as_mut().unwrap();
        let mut descriptor = libc::pollfd {
            fd: stderr.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = self.1.saturating_duration_since(std::time::Instant::now());

        // SAFETY: poll borrows one live descriptor for the duration of the call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis() as i32) };
        assert!(
            ready > 0,
            "test child did not produce stderr before its deadline"
        );

        let mut byte = [0];
        stderr.read_exact(&mut byte).unwrap();
        byte[0]
    }

    fn wait(mut self) -> std::process::Output {
        use std::time::{Duration, Instant};

        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < self.1, "test child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }

        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // SAFETY: spawn creates a group with this live/unreaped child's PID.
            // It cannot be reused by an unrelated process before wait reaps it.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

#[test]
fn fixtures_ignore_inherited_git_selection_and_configuration() {
    for selector_case in [false, true] {
        let r = Repo::new();
        let private = r.root.parent().unwrap();
        let hooks = private.join("foreign-hooks");
        fs::create_dir(&hooks).unwrap();
        let config = private.join("foreign-config");
        fs::write(
            &config,
            format!("[core]\n hooksPath = {}\n", hooks.display()),
        )
        .unwrap();

        let index = private.join("foreign-index");
        let original = fs::read(r.root.join(".git/index")).unwrap();
        fs::write(&index, &original).unwrap();

        for test in [
            "hook_failure_cleans_registered_worktree_and_retains_branch",
            "dirty_contents_deletions_staging_and_symlinks",
            "source_environment_is_honored_without_redirecting_destination_git",
        ] {
            // Deliberately contaminate the child test runner, not a fixture
            // command. Its own fixture initializer must neutralize these inputs.
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", test, "--nocapture"])
                .env("GIT_CONFIG_GLOBAL", &config)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_0", &hooks)
                .env("GIT_TEMPLATE_DIR", &hooks);

            if selector_case {
                child
                    .env("GIT_INDEX_FILE", &index)
                    .env("GIT_DIR", r.root.join(".git"))
                    .env("GIT_WORK_TREE", &r.root);
            }

            let output = TestChild::spawn(&mut child).wait();

            assert!(
                output.status.success(),
                "{test}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            assert_eq!(fs::read(&index).unwrap(), original);
            assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), original);
        }
    }
}

fn closed_output_stream() -> std::process::Stdio {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};

    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    std::process::Stdio::from(OwnedFd::from(writer))
}

#[test]
fn closed_stderr_rolls_back_partial_creation_without_panicking() {
    use std::os::unix::fs::PermissionsExt;

    for hook_fails in [false, true] {
        for no_clone in [false, true] {
            let r = Repo::new();
            fs::write(r.root.join(".env"), "environment").unwrap();
            let source_index = fs::read(r.root.join(".git/index")).unwrap();

            if hook_fails {
                let hook = r.root.join(".git/hooks/post-checkout");
                fs::write(&hook, "#!/bin/sh\necho hook-failure >&2\nexit 1\n").unwrap();
                fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
            }

            let destination = r.trees.join("closed-stderr");
            let mut child = r.command(env!("CARGO_BIN_EXE_git-wt"));
            child
                .current_dir(&r.root)
                .args(["new", "closed-stderr", "--path", ps(&destination)]);
            if no_clone {
                child.arg("--no-clone");
            }

            let output = child.stderr(closed_output_stream()).output().unwrap();

            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert!(!destination.exists());

            assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
            assert_eq!(
                fs::read_to_string(r.root.join("tracked")).unwrap(),
                "base\n"
            );

            r.git(
                &r.root,
                &["rev-parse", "--verify", "refs/heads/closed-stderr"],
            );

            assert_eq!(
                r.git(&r.root, &["worktree", "list", "--porcelain"])
                    .matches("worktree ")
                    .count(),
                1
            );
        }
    }
}

#[test]
fn closed_stdout_does_not_panic_or_undo_creation() {
    let r = Repo::new();
    fs::write(r.root.join(".env"), "environment").unwrap();
    let source_index = fs::read(r.root.join(".git/index")).unwrap();

    for subcommand in ["path", "list"] {
        for json in [false, true] {
            let mut child = r.command(env!("CARGO_BIN_EXE_git-wt"));
            child.current_dir(&r.root).arg(subcommand);
            if json {
                child.arg("--json");
            }

            let output = child.stdout(closed_output_stream()).output().unwrap();

            assert!(
                output.status.success(),
                "{:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
        }
    }

    for (n, no_clone) in [false, true].into_iter().enumerate() {
        for json in [false, true] {
            let branch = format!("closed-{n}-{json}");
            let destination = r.trees.join(&branch);
            let mut child = r.command(env!("CARGO_BIN_EXE_git-wt"));
            child
                .current_dir(&r.root)
                .args(["new", &branch, "--path", ps(&destination)]);
            if no_clone {
                child.arg("--no-clone");
            }
            if json {
                child.arg("--json");
            }

            let output = child.stdout(closed_output_stream()).output().unwrap();

            assert!(
                output.status.success(),
                "{:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );

            assert_eq!(
                r.git(&destination, &["symbolic-ref", "--short", "HEAD"]),
                branch
            );
            assert_eq!(destination.join(".env").exists(), !no_clone);

            assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
        }
    }
}

#[test]
fn ownership_inspection_failure_preserves_the_original_hook_error() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    let destination = r.trees.join("inaccessible");
    let source_index = fs::read(r.root.join(".git/index")).unwrap();

    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf 'UNIQUE_PRIMARY_HOOK_FAILURE\\n' >&2\nchmod 000 .\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

    let result = r.run(
        &r.root,
        &[
            "new",
            "inaccessible",
            "--no-clone",
            "--path",
            ps(&destination),
        ],
    );

    // Restore access before assertions so even a failing test can clean its fixture.
    if destination.is_dir() {
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let error = result.unwrap_err().to_string();
    assert!(error.contains("UNIQUE_PRIMARY_HOOK_FAILURE"), "{error}");

    assert!(
        error.contains("inspecting destination ownership"),
        "{error}"
    );
    assert!(error.contains("Permission denied"), "{error}");

    assert!(error.contains("cleanup failed"), "{error}");
    assert!(error.contains(ps(&destination)), "{error}");
    assert!(error.contains("branch inaccessible retained"), "{error}");

    assert!(destination.join(".git").is_file());
    assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
}

#[test]
fn hook_failure_cleans_registered_worktree_and_retains_branch() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf 'hook failed\\n' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let error = r
        .run(&r.root, &["new", "hook-failure", "--no-clone"])
        .unwrap_err();

    assert!(error.to_string().contains("hook failed"));
    assert!(error.to_string().contains("retained"));

    assert!(
        !r.trees
            .join("gitlab.example/group/team/repo/hook-failure")
            .exists()
    );

    assert_eq!(
        r.git(&r.root, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn base_start_points_reach_git_verbatim_so_tracking_is_configured() {
    let r = Repo::new();
    let initial = r.git(&r.root, &["rev-parse", "HEAD"]);
    fs::write(r.root.join("tracked"), "next\n").unwrap();
    r.git(&r.root, &["commit", "-qam", "next"]);
    r.git(
        &r.root,
        &["update-ref", "refs/remotes/origin/feature", &initial],
    );

    let output: Value = serde_json::from_str(
        &r.run(
            &r.root,
            &[
                "new",
                "tracking",
                "--no-clone",
                "--json",
                "--base",
                "origin/feature",
            ],
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(output["head"], initial);
    let tree = Path::new(output["path"].as_str().unwrap());
    assert_eq!(r.git(tree, &["rev-parse", "HEAD"]), initial);
    assert_eq!(
        r.git(&r.root, &["config", "branch.tracking.remote"]),
        "origin"
    );
    assert_eq!(
        r.git(&r.root, &["config", "branch.tracking.merge"]),
        "refs/heads/feature"
    );

    let plain = r.new_tree(&["new", "plain-base", "--no-clone", "--base", &initial]);
    assert_eq!(r.git(&plain, &["rev-parse", "HEAD"]), initial);
    assert!(
        r.run(
            &r.root,
            &["new", "bad-base", "--no-clone", "--base", "missing-ref"]
        )
        .is_err()
    );
}

#[test]
fn base_retries_a_retained_branch_only_at_the_same_commit() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new();
    let initial = r.git(&r.root, &["rev-parse", "HEAD"]);
    fs::write(r.root.join("tracked"), "next\n").unwrap();
    r.git(&r.root, &["commit", "-qam", "next"]);
    let next = r.git(&r.root, &["rev-parse", "HEAD"]);

    let hook = r.root.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf 'hook failed\\n' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let failure = r
        .run(&r.root, &["new", "retry", "--no-clone", "--base", &initial])
        .unwrap_err()
        .to_string();
    assert!(failure.contains("retained"), "{failure}");

    fs::remove_file(hook).unwrap();
    let tree = r.new_tree(&["new", "retry", "--no-clone", "--base", &initial]);
    assert_eq!(r.git(&tree, &["rev-parse", "HEAD"]), initial);

    r.git(&r.root, &["branch", "other"]);
    let output = r
        .command(env!("CARGO_BIN_EXE_git-wt"))
        .args(["new", "other", "--no-clone", "--base", &initial])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("another branch name"), "{error}");
    assert!(!error.contains("branch -D"), "{error}");
    assert_eq!(r.git(&r.root, &["rev-parse", "refs/heads/other"]), next);
    assert!(
        !r.trees
            .join("gitlab.example/group/team/repo/other")
            .exists()
    );
}

#[test]
fn retargeting_recognizes_any_alias_that_resolves_to_the_source_root() {
    use std::os::unix::fs::symlink;

    let r = Repo::new();
    let base = r.root.parent().unwrap();
    let alias = base.join("alias");
    symlink(&r.root, &alias).unwrap();
    fs::create_dir(base.join("elsewhere")).unwrap();
    fs::write(base.join("elsewhere/sentinel"), "outside").unwrap();

    fs::create_dir(r.root.join("cache")).unwrap();
    let links = [
        ("through-alias", alias.join("tracked"), Some("tracked")),
        (
            "dangling-alias",
            alias.join("missing/file"),
            Some("missing/file"),
        ),
        ("alias-root", alias.clone(), Some("")),
        ("alias-parent", alias.join("cache/../tracked"), None),
        ("outside", base.join("elsewhere/sentinel"), None),
    ];
    for (name, target, _) in &links {
        symlink(target, r.root.join("cache").join(name)).unwrap();
    }

    let tree = r.new_tree(&["new", "aliases"]);

    for (name, target, retargeted) in &links {
        let expected = match retargeted {
            Some(relative) => tree.join(relative),
            None => target.clone(),
        };
        assert_eq!(
            fs::read_link(tree.join("cache").join(name)).unwrap(),
            expected,
            "{name}"
        );
    }

    assert_eq!(
        fs::read_to_string(tree.join("cache/through-alias")).unwrap(),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(tree.join("cache/outside")).unwrap(),
        "outside"
    );
}

#[test]
fn missing_home_without_a_configured_root_is_reported_as_such() {
    let r = Repo::new();
    r.git(&r.root, &["config", "--unset", "gwt.root"]);

    let output = r
        .command(env!("CARGO_BIN_EXE_git-wt"))
        .args(["new", "homeless", "--no-clone"])
        .env_remove("HOME")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("HOME"), "{error}");
    assert!(!error.contains("gwt.root must be absolute"), "{error}");
    assert!(r.git(&r.root, &["branch", "--list", "homeless"]).is_empty());
}

#[test]
fn remove_prunes_empty_namespace_directories_below_the_repository_parent() {
    let r = Repo::new();
    let parent = r.trees.join("gitlab.example/group/team/repo");
    let a = r.new_tree(&["new", "feature/deep/a", "--no-clone"]);
    let b = r.new_tree(&["new", "feature/b", "--no-clone"]);

    r.run(&r.root, &["remove", "feature/deep/a"]).unwrap();
    assert!(!a.exists());
    assert!(!parent.join("feature/deep").exists());
    assert!(b.join(".git").is_file());

    fs::write(parent.join("feature/note"), "keep").unwrap();
    r.run(&r.root, &["remove", "feature/b"]).unwrap();
    assert!(!b.exists());
    assert_eq!(
        fs::read_to_string(parent.join("feature/note")).unwrap(),
        "keep"
    );

    fs::remove_file(parent.join("feature/note")).unwrap();
    let last = r.new_tree(&["new", "feature/last", "--no-clone"]);
    r.run(&r.root, &["remove", "feature/last"]).unwrap();
    assert!(!last.exists());
    assert!(!parent.join("feature").exists());
    assert!(parent.is_dir());

    let outside = r.trees.join("outside/tree");
    r.new_tree(&["new", "outside", "--no-clone", "--path", ps(&outside)]);
    r.run(&r.root, &["remove", "outside"]).unwrap();
    assert!(!outside.exists());
    assert!(r.trees.join("outside").is_dir());
}

#[test]
fn remove_never_prunes_the_repository_parent_or_its_ancestors() {
    let r = Repo::new();
    let parent = r.trees.join("gitlab.example/group/team/repo");

    let tree = r.new_tree(&["new", "whole-parent", "--no-clone", "--path", ps(&parent)]);
    assert_eq!(tree, parent);
    r.run(&r.root, &["remove", "whole-parent"]).unwrap();

    assert!(!parent.exists());
    assert!(
        parent.parent().unwrap().is_dir(),
        "namespace directory pruned"
    );
    assert!(r.trees.is_dir(), "gwt.root pruned");

    let sibling = r.trees.join("gitlab.example/group/team/other");
    let tree = r.new_tree(&["new", "sibling", "--no-clone", "--path", ps(&sibling)]);
    assert_eq!(tree, sibling);
    r.run(&r.root, &["remove", "sibling"]).unwrap();
    assert!(!sibling.exists());
    assert!(sibling.parent().unwrap().is_dir());
}

fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .unwrap()
}

#[test]
fn start_points_that_move_during_creation_are_refused_with_rollback() {
    use std::os::unix::fs::PermissionsExt;

    for (dirty, moved_ref) in [
        (false, "refs/remotes/origin/feature"),
        (true, "refs/heads/moving"),
    ] {
        let r = Repo::new();
        let initial = r.git(&r.root, &["rev-parse", "HEAD"]);
        fs::write(r.root.join("tracked"), "next\n").unwrap();
        r.git(&r.root, &["commit", "-qam", "next"]);
        let next = r.git(&r.root, &["rev-parse", "HEAD"]);
        r.git(&r.root, &["update-ref", moved_ref, &next]);
        fs::write(r.root.join("untracked"), "working").unwrap();
        let source_index = fs::read(r.root.join(".git/index")).unwrap();

        // A Git wrapper that moves the start point exactly when the worktree is
        // added, then defers to the installed Git. Nothing is emulated.
        let wrappers = r.root.parent().unwrap().join("wrappers");
        fs::create_dir(&wrappers).unwrap();
        fs::write(
            wrappers.join("git"),
            "#!/bin/sh\nif [ \"$1\" = worktree ] && [ \"$2\" = add ]; then\n  \"$GWT_TEST_REAL_GIT\" -C \"$GWT_TEST_SOURCE\" update-ref \"$GWT_TEST_MOVED_REF\" \"$GWT_TEST_MOVED_TO\"\nfi\nexec \"$GWT_TEST_REAL_GIT\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(wrappers.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(wrappers.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();

        let destination = r.trees.join("moving");
        let mut args = vec!["new", "moving", "--json", "--path", ps(&destination)];
        if dirty {
            args.push("--dirty");
        } else {
            args.extend(["--base", "origin/feature"]);
        }

        let output = r
            .command(env!("CARGO_BIN_EXE_git-wt"))
            .args(&args)
            .env("PATH", &path)
            .env("GWT_TEST_REAL_GIT", real_git())
            .env("GWT_TEST_SOURCE", &r.root)
            .env("GWT_TEST_MOVED_REF", moved_ref)
            .env("GWT_TEST_MOVED_TO", &initial)
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(1),
            "dirty={dirty}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "dirty={dirty}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("moved"), "dirty={dirty}: {error}");

        assert!(!destination.exists(), "dirty={dirty}");
        assert_eq!(
            r.git(&r.root, &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
        r.git(&r.root, &["rev-parse", "--verify", "refs/heads/moving"]);
        assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
        assert_eq!(r.git(&r.root, &["rev-parse", moved_ref]), initial);

        // The retained branch sits wherever Git left it. Settle the moved ref on
        // that commit, keep the wrapper's update a no-op, and the same
        // invocation succeeds at the commit it reports.
        let settled = if dirty {
            next.clone()
        } else {
            r.git(&r.root, &["rev-parse", "refs/heads/moving"])
        };
        r.git(&r.root, &["update-ref", moved_ref, &settled]);
        let output = r
            .command(env!("CARGO_BIN_EXE_git-wt"))
            .args(&args)
            .env("PATH", &path)
            .env("GWT_TEST_REAL_GIT", real_git())
            .env("GWT_TEST_SOURCE", &r.root)
            .env("GWT_TEST_MOVED_REF", moved_ref)
            .env("GWT_TEST_MOVED_TO", &settled)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "dirty={dirty}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["head"], settled);
        assert_eq!(r.git(&destination, &["rev-parse", "HEAD"]), settled);
    }
}

#[test]
fn signals_terminate_creation_without_success_output() {
    use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let r = Repo::new();
        let source_index = fs::read(r.root.join(".git/index")).unwrap();
        let hook = r.root.join(".git/hooks/post-checkout");
        fs::write(&hook, "#!/bin/sh\nawk 'BEGIN { for (i = 0; i < 20000; i++) print \"hook diagnostic line\" }' >&2\n").unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();

        let destination = r.trees.join("interrupted");
        let mut child = TestChild::spawn(r.command(env!("CARGO_BIN_EXE_git-wt")).args([
            "new",
            "interrupted",
            "--json",
            "--no-clone",
            "--path",
            ps(&destination),
        ]));

        // Git and its hook have finished; git-wt is blocked forwarding captured
        // diagnostics. Only the CLI is still running when it is signalled.
        child.read_stderr_byte();
        child.signal_process(signal);
        let output = child.wait();

        assert_eq!(output.status.signal(), Some(signal));
        assert!(output.stdout.is_empty());
        assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), source_index);
    }
}
