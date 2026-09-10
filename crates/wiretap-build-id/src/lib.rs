//! Stamps the commit a binary was built from into its version string.
//!
//! The package version cannot identify a build on its own: two binaries that
//! differ can both be `0.1.0`, and this project had exactly that — the daemon
//! soaking on the trial box and the one inside `v0.1.0-rc1` hash differently
//! and agreed on every string either could show. A soak report, or an operator
//! looking at a running box, has to be able to name the build it is about.
//!
//! Shared rather than copied into each build script, because the two would
//! otherwise have to be kept in step on the abbreviation length, the `-dirty`
//! marker and the override's precedence — three things nothing would check.
//!
//! Call [`emit`] from a `build.rs` and read the result with
//! `env!("WIRETAP_BUILD_ID")`.

use std::{path::Path, process::Command};

/// The calling crate's version and the commit it was built from, as
/// `0.1.0 (g36bfab1729af)`.
///
/// A macro rather than a shared `const`, because both halves have to be read in
/// the *calling* crate: `CARGO_PKG_VERSION` is that crate's own version, and
/// `WIRETAP_BUILD_ID` is what its own build script emitted. The format is
/// exactly the thing two binaries would drift on — the same argument that put
/// [`emit`] here rather than in each `build.rs`.
///
/// Requires [`emit`] to have run in the caller's build script.
#[macro_export]
macro_rules! build_version {
    () => {
        concat!(
            env!("CARGO_PKG_VERSION"),
            " (",
            env!("WIRETAP_BUILD_ID"),
            ")"
        )
    };
}

/// Emits `WIRETAP_BUILD_ID` for the calling crate, plus the rerun triggers
/// that keep it from going stale.
///
/// git is the source of truth whenever there is a checkout to ask.
/// `WIRETAP_BUILD_ID` in the environment is the fallback for a tree with no
/// `.git` — a `git archive` export, a source tarball, or a container build —
/// and does not override a resolvable HEAD. Failing both this reports
/// `unknown`, which `packaging/make-deb.sh` refuses to package.
pub fn emit() {
    println!("cargo:rerun-if-env-changed=WIRETAP_BUILD_ID");
    // A commit changes no file cargo watches on its own, so without this a
    // rebuild would go on stamping the commit before it.
    watch_git_refs();
    // And the calling crate's own sources, because an edit is exactly when the
    // `-dirty` marker stops being true. Watching git alone would be cheaper
    // per build and would let a package ship a binary claiming a clean commit
    // it was not built from, which is the failure this exists for. Relative,
    // so it resolves against whichever crate's build script called us.
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
