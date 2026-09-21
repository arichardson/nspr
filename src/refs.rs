//! Local refs mirroring each pull request's head branch.
//!
//! `nspr` keeps no metadata store — everything it knows lives in commit
//! trailers or is recomputed from the graph. That is deliberate (a metadata
//! store is a thing that can disagree with reality), but it does mean the
//! synthesized commits `nspr` pushes are not reachable from anything local,
//! so they are invisible to `git log`, `git range-diff` and friends, and are
//! eventually garbage collected.
//!
//! These refs fix that. They are strictly a convenience: nothing reads them
//! back, so deleting them all costs nothing but the ability to run
//!
//! ```text
//! git range-diff refs/nspr/pr/101 refs/nspr/pr/102
//! ```
//!
//! to see what changed between two revisions of a pull request.

use color_eyre::eyre::Result;
use git2::Oid;

use crate::git::Git;

pub fn ref_name(number: u64) -> String {
    format!("refs/nspr/pr/{number}")
}

pub fn root_ref_name(number: u64) -> String {
    format!("refs/nspr/root/{number}")
}

/// Point `refs/nspr/pr/<number>` at `oid`.
pub fn update(git: &Git, number: u64, oid: Oid) -> Result<()> {
    git.repo().reference(
        &ref_name(number),
        oid,
        true,
        "nspr: pull request head",
    )?;
    Ok(())
}

/// Record the initial commit (`c_0`) of a pull request's branch.
pub fn update_root(git: &Git, number: u64, root_oid: Oid) -> Result<()> {
    git.repo().reference(
        &root_ref_name(number),
        root_oid,
        true,
        "nspr: pull request root commit",
    )?;
    Ok(())
}

/// Look up the initial commit (`c_0`) of a pull request's branch, if recorded.
pub fn get_root(git: &Git, number: u64) -> Option<Oid> {
    git.repo()
        .find_reference(&root_ref_name(number))
        .ok()
        .and_then(|r| r.target())
}

/// Drop the refs for a pull request, once it is merged or closed.
pub fn remove(git: &Git, number: u64) -> Result<()> {
    if let Ok(mut r) = git.repo().find_reference(&ref_name(number)) {
        r.delete()?;
    }
    if let Ok(mut r) = git.repo().find_reference(&root_ref_name(number)) {
        r.delete()?;
    }
    Ok(())
}

/// Every pull request `nspr` currently has a ref for.
pub fn all(git: &Git) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    for name in git.repo().references_glob("refs/nspr/pr/*")?.names() {
        let name = name?;
        if let Some(n) = name.rsplit('/').next().and_then(|s| s.parse().ok()) {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}
