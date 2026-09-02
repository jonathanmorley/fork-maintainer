//! Tree composition — the *artifact* half of the engine (milestone 3).
//!
//! The fork's default branch `<X>` hosts the recomposed artifact: upstream
//! base tree **overlaid with** the fork-owned files. This module builds that
//! artifact on top of a given upstream mirror commit.
//!
//! # Fork-owned files
//!
//! A *fork-owned* file is one that exists in the fork's artifact but does not
//! exist upstream — e.g. the fork's `.github/` workflows, release scripts, or
//! documentation that deliberately diverges. When adopting new upstream
//! content, these files must be re-applied on top of the fresh upstream tree
//! so the fork keeps its proprietary additions.
//!
//! Detection direction: we diff the upstream mirror tree against the current
//! artifact tree. A change that is an *Addition* in that direction exists only
//! in the artifact, i.e. is fork-owned. Deletions and modifications (files the
//! fork removed or changed relative to upstream) are intentionally *not*
//! preserved for now — they represent the fork choosing to diverge from a
//! shared file, which is a policy decision for a later milestone.

use anyhow::Result;
use gix::{Repository, object::tree::EntryKind};
use std::ops::ControlFlow;

/// A file owned by the fork (present in the artifact, absent upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkOwnedFile {
    /// The path relative to the repository root, e.g. `.github/workflows/ci.yml`.
    pub path: String,
    /// The object id of the file's blob.
    pub oid: gix::ObjectId,
    /// The kind of entry (blob, executable, link, …).
    pub kind: EntryKind,
}

/// Enumerate the files the fork owns relative to `upstream_mirror_ref`, by
/// diffing the current `artifact_ref` tree against the upstream mirror tree.
///
/// `upstream_mirror_ref` is the `upstream/<X>` mirror; `artifact_ref` is the
/// fork's current default-branch ref.
pub fn fork_owned_files(
    repo: &Repository,
    upstream_mirror_ref: &str,
    artifact_ref: &str,
) -> Result<Vec<ForkOwnedFile>> {
    let upstream_tree = tree_at_ref(repo, upstream_mirror_ref)?;
    let artifact_tree = tree_at_ref(repo, artifact_ref)?;

    let mut owned = Vec::new();
    let mut platform = upstream_tree.changes()?;
    platform.for_each_to_obtain_tree(&artifact_tree, |change| {
        use gix::object::tree::diff::Change::*;
        // Addition (upstream -> artifact) => file exists only in the fork.
        if let Addition {
            location,
            entry_mode,
            id,
            ..
        } = change
        {
            let kind: EntryKind = entry_mode.kind();
            // The diff reports added subtrees alongside their leaf entries; we
            // only want the leaf files (the subtree entries are containers).
            if kind != EntryKind::Tree {
                owned.push(ForkOwnedFile {
                    path: location.to_string(),
                    oid: id.detach(),
                    kind,
                });
            }
        }
        Ok::<_, anyhow::Error>(ControlFlow::Continue(()))
    })?;

    owned.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(owned)
}

/// The outcome of recomposing the artifact branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeOutcome {
    /// The new artifact tree id.
    pub tree: gix::ObjectId,
    /// The new artifact commit id.
    pub commit: gix::ObjectId,
    /// The number of fork-owned files that were re-applied on top of the
    /// upstream base.
    pub fork_files_applied: usize,
}

/// Recompose the artifact branch `<X>` on top of the upstream mirror commit,
/// preserving fork-owned files.
///
/// Builds a new tree whose base is the upstream mirror tree with all fork-owned
/// files re-applied, then writes a new commit whose parent is the upstream
/// mirror commit, and finally points the artifact ref at it.
pub fn compose_artifact(
    repo: &Repository,
    upstream_mirror_ref: &str,
    artifact_ref: &str,
    committer: gix::actor::SignatureRef<'_>,
) -> Result<ComposeOutcome> {
    let owned = fork_owned_files(repo, upstream_mirror_ref, artifact_ref)?;

    // The base is the upstream mirror tree.
    let upstream_oid = repo.find_reference(upstream_mirror_ref)?.id().detach();
    let upstream_commit = repo.find_commit(upstream_oid)?;
    let upstream_tree_id = upstream_commit.tree_id()?.detach();
    let upstream_tree = repo.find_tree(upstream_tree_id)?;

    let mut editor = repo.edit_tree(upstream_tree.id)?;
    for file in &owned {
        editor.upsert(file.path.as_str(), file.kind, file.oid)?;
    }
    let new_tree = editor.write()?.detach();

    let new_commit = repo
        .new_commit_as(
            committer,
            committer,
            format!(
                "Recompose fork artifact on upstream ({} fork-owned files)",
                owned.len()
            ),
            new_tree,
            [upstream_oid],
        )?
        .id;

    write_artifact_ref(repo, artifact_ref, new_commit)?;

    Ok(ComposeOutcome {
        tree: new_tree,
        commit: new_commit,
        fork_files_applied: owned.len(),
    })
}

/// Read the tree that a ref's commit points to.
fn tree_at_ref<'r>(repo: &'r Repository, ref_name: &str) -> Result<gix::Tree<'r>> {
    let oid = repo.find_reference(ref_name)?.id().detach();
    let commit = repo.find_commit(oid)?;
    let tree_id = commit.tree_id()?.detach();
    Ok(repo.find_tree(tree_id)?)
}

/// Point `artifact_ref` at `commit` unconditionally (the artifact is a
/// recomposed history, rewrites are normal).
fn write_artifact_ref(repo: &Repository, artifact_ref: &str, commit: gix::ObjectId) -> Result<()> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::Target;

    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("fork-maintainer: recompose artifact to {commit}").into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(commit),
        },
        name: gix::refs::FullName::try_from(artifact_ref)?,
        deref: false,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::actor::SignatureRef;
    use gix::objs::tree::EntryKind;
    use gix::refs::transaction::PreviousValue;
    use std::path::PathBuf;

    const SIG: &[u8] = b"tester <tester@example.com> 1711398853 +0000";

    fn sig() -> SignatureRef<'static> {
        SignatureRef::from_bytes(SIG).expect("valid sig")
    }

    fn init_bare(path: &std::path::Path) -> Repository {
        gix::init_bare(path).expect("init bare")
    }

    /// Write a commit whose tree contains the given `files` (name -> content),
    /// and return its ObjectId.
    fn commit_with_files(
        repo: &Repository,
        files: &[(&str, &str)],
        message: &str,
        parent: Option<gix::ObjectId>,
    ) -> gix::ObjectId {
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        for (name, content) in files {
            let blob = repo.write_blob(content).expect("write blob");
            editor
                .upsert(*name, EntryKind::Blob, blob.detach())
                .expect("upsert");
        }
        let tree_id = editor.write().expect("write tree").detach();
        repo.new_commit_as(sig(), sig(), message, tree_id, parent)
            .expect("new commit")
            .id
    }

    fn ref_id(repo: &Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    fn set_ref(repo: &Repository, name: &str, target: gix::ObjectId) {
        repo.reference(name, target, PreviousValue::Any, "set ref for test")
            .expect("set ref");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Read a path's blob content from a tree, if present.
    fn tree_blob(repo: &Repository, tree_id: gix::ObjectId, path: &str) -> Option<String> {
        let mut tree = repo.find_tree(tree_id).expect("find tree");
        tree.peel_to_entry(path.split('/'))
            .expect("peel")
            .map(|entry| {
                let blob = repo.find_blob(entry.oid().to_owned()).expect("find blob");
                String::from_utf8_lossy(&blob.data).into_owned()
            })
    }

    #[test]
    fn detects_fork_owned_files() {
        let dir = temp_dir("detects_fork_owned");
        let repo = init_bare(&dir);

        // Upstream mirror has a.txt only.
        let up = commit_with_files(&repo, &[("a.txt", "upstream a")], "upstream", None);
        set_ref(&repo, "refs/heads/upstream/main", up);

        // Artifact has a.txt (same) plus a fork-only file.
        let art = commit_with_files(&repo, &[("a.txt", "upstream a"), ("fork-only.txt", "fork")], "artifact", None);
        set_ref(&repo, "refs/heads/main", art);

        let owned = fork_owned_files(&repo, "refs/heads/upstream/main", "refs/heads/main").expect("detect");
        assert_eq!(owned.len(), 1, "expected exactly one fork-owned file: {owned:?}");
        assert_eq!(owned[0].path, "fork-only.txt");
    }

    #[test]
    fn detects_no_fork_owned_when_identical() {
        let dir = temp_dir("detects_no_fork_owned");
        let repo = init_bare(&dir);

        let up = commit_with_files(&repo, &[("a.txt", "a")], "upstream", None);
        set_ref(&repo, "refs/heads/upstream/main", up);
        let art = commit_with_files(&repo, &[("a.txt", "a")], "artifact", None);
        set_ref(&repo, "refs/heads/main", art);

        let owned = fork_owned_files(&repo, "refs/heads/upstream/main", "refs/heads/main").expect("detect");
        assert!(owned.is_empty(), "expected no fork-owned files: {owned:?}");
    }

    #[test]
    fn compose_preserves_fork_owned_on_new_upstream_base() {
        let dir = temp_dir("compose_preserves");
        let repo = init_bare(&dir);

        // Upstream evolution: c1 has a.txt, c2 has a.txt+b.txt.
        let c1 = commit_with_files(&repo, &[("a.txt", "a1")], "upstream c1", None);
        set_ref(&repo, "refs/heads/upstream/main", c1);
        let c2 = commit_with_files(&repo, &[("a.txt", "a1"), ("b.txt", "b2")], "upstream c2", Some(c1));
        set_ref(&repo, "refs/heads/upstream/main", c2);

        // The existing artifact was composed on top of c1 and carries a
        // fork-owned file `.github/ci.yml`.
        let art = commit_with_files(
            &repo,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "artifact on c1",
            Some(c1),
        );
        set_ref(&repo, "refs/heads/main", art);

        // Recompose: the new artifact should adopt the new upstream content
        // (b.txt) while keeping the fork-owned file.
        let outcome = compose_artifact(
            &repo,
            "refs/heads/upstream/main",
            "refs/heads/main",
            sig(),
        )
        .expect("compose");

        assert_eq!(outcome.fork_files_applied, 1);
        // New artifact tree still contains a.txt, b.txt, and the fork file.
        assert_eq!(tree_blob(&repo, outcome.tree, "a.txt").as_deref(), Some("a1"));
        assert_eq!(tree_blob(&repo, outcome.tree, "b.txt").as_deref(), Some("b2"));
        assert_eq!(
            tree_blob(&repo, outcome.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
        // Artifact ref advanced to the new commit.
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(outcome.commit));
    }
}

