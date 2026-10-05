//! Local bare-repository fixtures for git-backed unit tests (pin store,
//! workspace manager). `file://` URLs only — no network.

use std::path::Path;
use tempfile::TempDir;

/// A one-action, one-task workspace whose script prints `marker`.
pub(crate) fn workflow(marker: &str) -> String {
    format!(
        "actions:\n  greet:\n    type: script\n    script: echo {marker}\n\
         tasks:\n  hello:\n    flow:\n      s1:\n        action: greet\n"
    )
}

/// A bare repo whose `main` has one commit holding `files`.
/// Returns (dir, `file://` url, commit sha).
pub(crate) fn bare_remote(files: &[(&str, &str)]) -> (TempDir, String, String) {
    let dir = TempDir::new().unwrap();
    let repo = git2::Repository::init_bare(dir.path()).unwrap();
    let mut tb = repo.treebuilder(None).unwrap();
    for &(name, content) in files {
        let blob = repo.blob(content.as_bytes()).unwrap();
        tb.insert(name, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(tb.write().unwrap()).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let oid = repo
        .commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let url = format!("file://{}", dir.path().display());
    (dir, url, oid.to_string())
}

/// Commit `files` on `branch`, branching from `main` when `branch` does not
/// exist yet. Returns the new commit sha.
pub(crate) fn commit_on(repo_path: &Path, branch: &str, files: &[(&str, &str)]) -> String {
    let repo = git2::Repository::open_bare(repo_path).unwrap();
    let refname = format!("refs/heads/{branch}");
    let parent = repo
        .find_reference(&refname)
        .or_else(|_| repo.find_reference("refs/heads/main"))
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut tb = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    for &(name, content) in files {
        let blob = repo.blob(content.as_bytes()).unwrap();
        tb.insert(name, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(tb.write().unwrap()).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    repo.commit(Some(&refname), &sig, &sig, "change", &tree, &[&parent])
        .unwrap()
        .to_string()
}

pub(crate) fn lightweight_tag(repo_path: &Path, name: &str, commit: &str) {
    let repo = git2::Repository::open_bare(repo_path).unwrap();
    let oid = git2::Oid::from_str(commit).unwrap();
    repo.reference(&format!("refs/tags/{name}"), oid, true, "tag")
        .unwrap();
}

/// Returns the TAG OBJECT id (not the commit).
pub(crate) fn annotated_tag(repo_path: &Path, name: &str, commit: &str) -> String {
    let repo = git2::Repository::open_bare(repo_path).unwrap();
    let target = repo
        .find_object(git2::Oid::from_str(commit).unwrap(), None)
        .unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    repo.tag(name, &target, &sig, "release", false)
        .unwrap()
        .to_string()
}

pub(crate) fn delete_branch(repo_path: &Path, branch: &str) {
    let repo = git2::Repository::open_bare(repo_path).unwrap();
    repo.find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .delete()
        .unwrap();
}
