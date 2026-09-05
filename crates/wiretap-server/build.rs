//! Stamps the commit the binary was built from into `--version`.
//!
//! The package version cannot identify a build on its own: two binaries that
//! differ can both be `0.1.0`, and this project had exactly that — the daemon
//! soaking on the trial box and the one in the first release candidate hash
//! differently and agreed on every string either could show. A soak report has
//! to be able to name the build it is about.
//!
//! git is the source of truth whenever there is a checkout to ask.
//! `WIRETAP_BUILD_ID` is the fallback for a tree that has no `.git` — a
//! `git archive` export, or a source tarball — and failing both this reports
//! `unknown`, which `packaging/make-deb.sh` refuses to package.

use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=WIRETAP_BUILD_ID");
    // A commit changes no file cargo watches on its own, so without this a
    // rebuild would go on stamping the commit before it.
    watch_git_refs();
    // And the crate's own sources, because an edit is exactly when the
    // `-dirty` marker stops being true. Watching git alone would be cheaper
    // per build and would let `make-deb.sh` ship a binary claiming a clean
    // commit it was not built from, which is the failure this file exists for.
    println!("cargo:rerun-if-changed=src");

    let id = git_id()
        .or_else(env_build_id)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=WIRETAP_BUILD_ID={id}");
}

/// The commit, and whether the tree built from had uncommitted changes.
fn git_id() -> Option<String> {
    let sha = git(&["rev-parse", "--short=12", "HEAD"])?;
    // Not `git describe --dirty`: describe wants a tag, and this repo's first
    // tag is newer than nearly all of its history.
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    Some(format!("g{sha}{}", if dirty { "-dirty" } else { "" }))
}

/// Only consulted when git could not answer. An empty value is not an answer
/// either — it would stamp `0.1.0 ()` and read as a build that named itself.
fn env_build_id() -> Option<String> {
    std::env::var("WIRETAP_BUILD_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn watch_git_refs() {
    let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let dir = Path::new(&dir);
    let head = dir.join("HEAD");
    let Ok(contents) = std::fs::read_to_string(&head) else {
        return;
    };
    println!("cargo:rerun-if-changed={}", head.display());

    // `ref: refs/heads/main` on a branch, a bare sha when detached, and a
    // commit rewrites that ref file. Only if it exists: naming a missing path
    // makes cargo rerun this on every build. Watching `packed-refs` when it
    // does not would buy nothing — a commit writes a loose ref rather than
    // touching the packed file, so the watch would never fire. In a repository
    // whose refs are packed, `src` above is the backstop.
    let Some(r) = contents.trim().strip_prefix("ref: ") else {
        return;
    };
    let branch = dir.join(r);
    if branch.exists() {
        println!("cargo:rerun-if-changed={}", branch.display());
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
