# git-wt

Create Git worktrees with your local environment ready to use. Ignored files
such as `.env`, `node_modules`, and build caches are carried over using APFS
clones, which share file data until you change it. Requires macOS and APFS.

```sh
git wt new fix-auth          # create a worktree for a new or existing branch
git wt new                   # choose a new branch name, e.g. main-1
git wt new experiment --dirty # also carry uncommitted changes
git wt list                  # list worktrees (alias: ls)
git wt path fix-auth         # print its path
git wt remove fix-auth       # remove the worktree, keep the branch (alias: rm)
```

`new` prints the new directory's path. Changing directories and selecting
worktrees are up to your shell. Use `--json` with `new`, `list`, or `path` for scripts.

Worktrees go under `~/worktrees/<forge>/<owner>/<repo>/<branch>`, preserving
longer repository namespaces. Set a different base directory in Git config:

```gitconfig
[gwt]
    root = ~/tmp/worktrees

[gwt "ssh://git@git.company.example"]
    root = ~/tmp/company/worktrees
```

The scoped URL must match your remote's scheme and host. The default remote is
`origin`; change it with `gwt.remote`. Use `--path` to choose a directory directly.

```sh
git wt new fix-auth --path ../fix-auth
git wt new fix-auth --exclude node_modules
git wt new fix-auth --no-clone
```

Existing destination files are kept. Ignored files that disappear during
creation are skipped. `--dirty` carries working changes without preserving
staging. `--no-clone` creates an ordinary worktree without carrying local files.
Use `--force` with `remove` to discard uncommitted files and changes.

Git handles branches, merging, and pushing as usual. Run `git wt -h` for help.

## Install from source

```sh
nix develop
cargo install --path . --locked
```

If Rust and Git are already installed, skip `nix develop`.
