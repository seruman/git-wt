//! Forge-path fixtures adapted from ghq.
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

use git_wt::naming::remote_path;
use std::path::Path;

#[test]
fn ghq_compatible_forge_paths() {
    let url = "git@gitlab.com:group/subgroup/repo.git";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("gitlab.com/group/subgroup/repo"),
        "{url}"
    );

    let url = "ssh://git@stash.example:7999/TEAM/repo.git";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("stash.example/TEAM/repo"),
        "{url}"
    );

    let url = "git@git.sr.ht:~alice/repo";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("git.sr.ht/~alice/repo"),
        "{url}"
    );

    let url = "https://dev.azure.com/org/project/_git/repo";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("dev.azure.com/org/project/_git/repo"),
        "{url}"
    );

    let url = "git@ssh.dev.azure.com:v3/org/project/repo";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("ssh.dev.azure.com/v3/org/project/repo"),
        "{url}"
    );

    let url = "https://bitbucket.local:8888/motemen/ghq.git";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("bitbucket.local/motemen/ghq"),
        "{url}"
    );

    let url = "https://git.code.sf.net/p/ghq/code";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("git.code.sf.net/p/ghq/code"),
        "{url}"
    );

    let url = "ssh://git@stash.com/scm/motemen/ghq.git";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("stash.com/scm/motemen/ghq"),
        "{url}"
    );

    let url = "https://example.com/team/repo.git.git";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("example.com/team/repo.git"),
        "{url}"
    );

    let url = "https://example.com/team/hello%20world.git";
    assert_eq!(
        remote_path(url).unwrap().1,
        Path::new("example.com/team/hello world"),
        "{url}"
    );
}

#[test]
fn unsupported_identity_needs_explicit_path() {
    for url in ["../repo", "/home/me/repo", "file:///tmp/repo"] {
        assert!(remote_path(url).is_err(), "{url}");
    }

    let url = "https://example.com";
    assert!(remote_path(url).is_err(), "{url}");

    let url = "codecommit::region://profile@repo";
    assert!(remote_path(url).is_err(), "{url}");
}
