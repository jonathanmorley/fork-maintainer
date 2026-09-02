//! Artifact composition — the fork's default branch as a uniform stack overlay.
//!
//! The fork's artifact `<X>` is **upstream base tree + an ordered stack of
//! fork branches**, rebased onto the upstream mirror tip. There is no special
//! notion of "fork-owned files" as a separate mechanism: a fork's persistent
//! overlays — its `.github/` workflows, release scripts, fork-specific docs —
//! are just *another branch in the stack* (conceptually an open PR against
//! `upstream/<X>` that is never merged upstream). It is the bottom layer.
//!
//! Composition is therefore one uniform operation: take the upstream base and
//! layer each branch's changes on top, in order. Because we build the artifact
//! fresh from the upstream base every cycle, the branch is effectively *reset*
//! to upstream — any ad-hoc edits made directly on `<X>` are discarded and
//! must instead be made on a stack branch.
//!
//! # Seam
//!
//! Which branches form the stack, and in what order, is decided upstream of
//! this module (discovered from open PRs by the GitHub layer, or supplied
//! explicitly). This module only needs an ordered list of refs and the base
//! ref; it is pure git and fully testable against local bare repositories.
//!
//! # Semantics
//!
//! This is a deterministic *tree overlay*, not a 3-way merge: each path is
//! last-write-wins based on the order changes are applied. It does not detect
//! or resolve conflicts. That is a deliberate trade-off for this milestone —
//! a full cascade-rebase with conflict resolution is a later, larger piece
//! (see the `Rebase` trait plan).
//!
//! Rewrite (rename/copy) tracking is disabled for the diff so we get clean
//! per-path additions, deletions, and modifications, which is what an overlay
//! needs.

use anyhow::Result;
use gix::{Repository, object::tree::EntryKind};
use std::ops::ControlFlow;

/// A single path-level change that, applied to a tree, makes it match the
/// target of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathChange {
    /// Add or modify the entry at `path` (addition and modification are the
    /// same from the overlay's perspective: write the target's oid+kind).
    Upsert {
        /// The repository-relative path, e.g. `src/lib.rs`.
        path: String,
        /// The object id of the blob/tree to write.
        oid: gix::ObjectId,
        /// The kind of entry (blob, executable, link, …).
        kind: EntryKind,
    },
    /// Remove the entry at `path`.
    Remove {
        /// The repository-relative path.
        path: String,
    },
}

impl PathChange {
    fn path(&self) -> &str {
        match self {
            PathChange::Upsert { path, .. } | PathChange::Remove { path } => path,
        }
    }
}

/// Enumerate the path-level changes that transform the tree at `from_ref` into
/// the tree at `to_ref`, in a deterministic order (sorted by path).
///
/// With rewrite tracking disabled, this yields clean [`Upsert`](PathChange::Upsert)
/// and [`Remove`](PathChange::Remove) events; intermediate tree entries
/// (directories) are dropped since their leaf entries are reported separately.
pub fn patch_changes(
    repo: &Repository,
    from_ref: &str,
    to_ref: &str,
) -> Result<Vec<PathChange>> {
    let from_tree = tree_at_ref(repo, from_ref)?;
    let to_tree = tree_at_ref(repo, to_ref)?;
    tree_changes(&from_tree, &to_tree)
}

/// Diff two trees, producing deterministic path-level [`PathChange`]s.
///
/// With rewrite tracking disabled this yields clean upsert/remove events;
/// intermediate tree entries (directories) are dropped since their leaf
/// entries are reported separately.
fn tree_changes(
    from_tree: &gix::Tree<'_>,
    to_tree: &gix::Tree<'_>,
) -> Result<Vec<PathChange>> {
    let mut changes = Vec::new();
    let mut platform = from_tree.changes()?;
    platform.options(|opts| {
        opts.track_rewrites(None);
    });
    platform.for_each_to_obtain_tree(to_tree, |change| {
        use gix::object::tree::diff::Change::*;
        match change {
            Addition {
                location,
                entry_mode,
                id,
                ..
            }
            | Modification {
                location,
                entry_mode,
                id,
                ..
            } => {
                let kind: EntryKind = entry_mode.kind();
                // Skip added/modified subtrees; their leaf entries come back
                // as their own changes.
                if kind != EntryKind::Tree {
                    changes.push(PathChange::Upsert {
                        path: location.to_string(),
                        oid: id.detach(),
                        kind,
                    });
                }
            }
            Deletion { location, .. } => {
                changes.push(PathChange::Remove {
                    path: location.to_string(),
                });
            }
            // Unreachable while rewrite tracking is disabled.
            Rewrite { .. } => {}
        }
        Ok::<_, anyhow::Error>(ControlFlow::Continue(()))
    })?;

    changes.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(changes)
}

/// Apply a set of `changes` to `base_tree`, returning the new tree id.
///
/// `[`PathChange::Upsert`]` writes the entry (creating any intermediate
/// directories); `[`PathChange::Remove`]` deletes it.
pub fn apply_changes(
    repo: &Repository,
    base_tree: gix::ObjectId,
    changes: &[PathChange],
) -> Result<gix::ObjectId> {
    let base = repo.find_tree(base_tree)?;
    let mut editor = repo.edit_tree(base.id)?;
    for change in changes {
        match change {
            PathChange::Upsert { path, oid, kind } => {
                editor.upsert(path.as_str(), *kind, *oid)?;
            }
            PathChange::Remove { path } => {
                editor.remove_leaf(path.as_str())?;
            }
        }
    }
    Ok(editor.write()?.detach())
}

/// The outcome of composing a branch stack onto a base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackOutcome {
    /// The final composed tree id.
    pub tree: gix::ObjectId,
    /// The stack artifact commit id.
    pub commit: gix::ObjectId,
    /// The number of stack branches that were layered onto the base.
    pub patches_applied: usize,
}

/// Compose the artifact `<X>` from an ordered stack of fork branches layered
/// on `base_ref` (the `upstream/<X>` mirror), and point `target_ref` at the
/// result.
///
/// This is the single artifact-composition entry point. `branches` is an
/// ordered list of refs forming the cascade — the fork-owned branch first,
/// then the patch PRs.
///
/// Each branch's changes are computed against its **own base** — the tree of
/// its head commit's first parent — rather than against the running tree.
/// This is what makes the overlay correct when upstream has advanced: files
/// that upstream added *after* a branch forked are not misread as deletions
/// the branch made. The branch's own edits (additions, deletions, and
/// modifications of files present at its fork point) are re-applied, in order,
/// onto the running composed tree, which starts as the `base_ref` tree (i.e.
/// `<X>` is reset to upstream). A new commit is written whose parent is
/// `base_ref`'s commit, and `target_ref` is advanced to it.
///
/// Because the tree is rebuilt from `base_ref` every cycle, ad-hoc edits made
/// directly on the artifact are discarded; persistent fork content must live
/// on a stack branch.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if a branch ref or the base ref cannot be
/// resolved, or if the tree editor fails.
pub fn compose(
    repo: &Repository,
    base_ref: &str,
    branches: &[String],
    target_ref: &str,
    committer: gix::actor::SignatureRef<'_>,
) -> Result<StackOutcome> {
    let (base_tree_id, base_commit) = peel_tree_and_commit(repo, base_ref)?;
    let mut running = base_tree_id;

    for branch in branches {
        // The branch head commit's own base tree (first-parent) is what the
        // branch was forked from; diff against it so upstream-only changes
        // made after the fork are not attributed to the branch.
        let (head_tree, head_oid) = peel_tree_and_commit(repo, branch)?;
        let head = repo.find_commit(head_oid)?;
        let branch_base = match head.parent_ids().next() {
            Some(parent) => repo.find_commit(parent)?.tree_id()?.detach(),
            None => repo.empty_tree().id,
        };

        let from_tree = repo.find_tree(branch_base)?;
        let to_tree = repo.find_tree(head_tree)?;
        let changes = tree_changes(&from_tree, &to_tree)?;
        running = apply_changes(repo, running, &changes)?;
    }

    let commit = repo
        .new_commit_as(
            committer,
            committer,
            format!(
                "Recompose artifact from {} stack branches",
                branches.len()
            ),
            running,
            [base_commit],
        )?
        .id;

    write_ref(repo, target_ref, commit)?;

    Ok(StackOutcome {
        tree: running,
        commit,
        patches_applied: branches.len(),
    })
}

/// Read the tree and commit id that a ref's commit points to.
fn peel_tree_and_commit(repo: &Repository, ref_name: &str) -> Result<(gix::ObjectId, gix::ObjectId)> {
    let oid = repo.find_reference(ref_name)?.id().detach();
    let commit = repo.find_commit(oid)?;
    Ok((commit.tree_id()?.detach(), oid))
}

/// Read the tree that a ref's commit points to.
fn tree_at_ref<'r>(repo: &'r Repository, ref_name: &str) -> Result<gix::Tree<'r>> {
    let oid = repo.find_reference(ref_name)?.id().detach();
    let commit = repo.find_commit(oid)?;
    Ok(repo.find_tree(commit.tree_id()?.detach())?)
}

/// Point `target_ref` at `commit` unconditionally (stack artifact rewrites are
/// normal).
fn write_ref(repo: &Repository, target_ref: &str, commit: gix::ObjectId) -> Result<()> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::Target;

    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("fork-maintainer: recompose stack artifact to {commit}").into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(commit),
        },
        name: gix::refs::FullName::try_from(target_ref)?,
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

    fn tree_blob(repo: &Repository, tree_id: gix::ObjectId, path: &str) -> Option<String> {
        let mut tree = repo.find_tree(tree_id).expect("find tree");
        tree.peel_to_entry(path.split('/'))
            .expect("peel")
            .map(|entry| {
                let blob = repo.find_blob(entry.oid().to_owned()).expect("find blob");
                String::from_utf8_lossy(&blob.data).into_owned()
            })
    }

    fn tree_has_entry(repo: &Repository, tree_id: gix::ObjectId, path: &str) -> bool {
        let mut tree = repo.find_tree(tree_id).expect("find tree");
        tree.peel_to_entry(path.split('/')).expect("peel").is_some()
    }

    #[test]
    fn enumerates_add_remove_modify_changes() {
        let dir = temp_dir("changes");
        let repo = init_bare(&dir);

        // Base: a.txt and gone.txt.
        let base = commit_with_files(&repo, &[("a.txt", "a1"), ("gone.txt", "g")], "base", None);
        set_ref(&repo, "refs/heads/base", base);
        // Target: a.txt modified, b.txt added, gone.txt removed.
        let target = commit_with_files(
            &repo,
            &[("a.txt", "a2"), ("b.txt", "b1")],
            "target",
            Some(base),
        );
        set_ref(&repo, "refs/heads/target", target);

        let changes =
            patch_changes(&repo, "refs/heads/base", "refs/heads/target").expect("changes");
        let upserts: Vec<_> = changes
            .iter()
            .filter_map(|c| match c {
                PathChange::Upsert { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        let removes: Vec<_> = changes
            .iter()
            .filter_map(|c| match c {
                PathChange::Remove { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(upserts, vec!["a.txt", "b.txt"]);
        assert_eq!(removes, vec!["gone.txt"]);
    }

    #[test]
    fn applies_changes_to_tree() {
        let dir = temp_dir("apply");
        let repo = init_bare(&dir);

        let base = commit_with_files(&repo, &[("a.txt", "a1"), ("gone.txt", "g")], "base", None);
        set_ref(&repo, "refs/heads/base", base);
        let target = commit_with_files(
            &repo,
            &[("a.txt", "a2"), ("b.txt", "b1")],
            "target",
            Some(base),
        );
        set_ref(&repo, "refs/heads/target", target);

        let changes = patch_changes(&repo, "refs/heads/base", "refs/heads/target").unwrap();
        let base_tree = repo
            .find_commit(base)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let new_tree = apply_changes(&repo, base_tree, &changes).unwrap();

        assert_eq!(tree_blob(&repo, new_tree, "a.txt").as_deref(), Some("a2"));
        assert_eq!(tree_blob(&repo, new_tree, "b.txt").as_deref(), Some("b1"));
        assert!(!tree_has_entry(&repo, new_tree, "gone.txt"));
    }

    #[test]
    fn compose_full_artifact_from_stack() {
        let dir = temp_dir("stack");
        let repo = init_bare(&dir);

        // Base: upstream mirror with a.txt.
        let base = commit_with_files(&repo, &[("a.txt", "a1")], "base", None);
        set_ref(&repo, "refs/heads/upstream/main", base);

        // Fork-owned branch: the fork's persistent overlays (its bottom layer
        // of the stack, conceptually an open PR that is never merged upstream).
        let owned = commit_with_files(
            &repo,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "owned",
            Some(base),
        );
        set_ref(&repo, "refs/heads/fork-owned", owned);

        // P1 targets the fork-owned layer, adding helper.txt.
        let p1 = commit_with_files(
            &repo,
            &[
                ("a.txt", "a1"),
                (".github/ci.yml", "workflow"),
                ("helper.txt", "h"),
            ],
            "P1",
            Some(owned),
        );
        set_ref(&repo, "refs/heads/patch/1", p1);

        // P2 builds on P1, adding feature.txt and modifying a.txt.
        let p2 = commit_with_files(
            &repo,
            &[
                ("a.txt", "a2"),
                (".github/ci.yml", "workflow"),
                ("helper.txt", "h"),
                ("feature.txt", "f"),
            ],
            "P2",
            Some(p1),
        );
        set_ref(&repo, "refs/heads/patch/2", p2);

        let branches = vec![
            "refs/heads/fork-owned".to_string(),
            "refs/heads/patch/1".to_string(),
            "refs/heads/patch/2".to_string(),
        ];
        let outcome = compose(
            &repo,
            "refs/heads/upstream/main",
            &branches,
            "refs/heads/main",
            sig(),
        )
        .expect("compose stack");

        assert_eq!(outcome.patches_applied, 3);
        // Final tree carries the fork-owned overlay AND the cumulative patches.
        assert_eq!(
            tree_blob(&repo, outcome.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
        assert_eq!(tree_blob(&repo, outcome.tree, "a.txt").as_deref(), Some("a2"));
        assert_eq!(tree_blob(&repo, outcome.tree, "helper.txt").as_deref(), Some("h"));
        assert_eq!(
            tree_blob(&repo, outcome.tree, "feature.txt").as_deref(),
            Some("f")
        );
        // Target ref advanced to the stack commit.
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(outcome.commit));
    }

    #[test]
    fn recompose_discards_adhoc_edits_on_artifact() {
        let dir = temp_dir("reset_artifact");
        let repo = init_bare(&dir);

        // Upstream base.
        let base = commit_with_files(&repo, &[("a.txt", "a1")], "base", None);
        set_ref(&repo, "refs/heads/upstream/main", base);

        // Fork-owned branch (the only stack member).
        let owned = commit_with_files(
            &repo,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "owned",
            Some(base),
        );
        set_ref(&repo, "refs/heads/fork-owned", owned);

        let branches = vec!["refs/heads/fork-owned".to_string()];
        let first = compose(
            &repo,
            "refs/heads/upstream/main",
            &branches,
            "refs/heads/main",
            sig(),
        )
        .expect("first compose");
        assert_eq!(
            tree_blob(&repo, first.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );

        // Someone hand-edits the artifact directly: adds scratch.txt (an ad-hoc
        // manual change that is NOT in any stack branch).
        let scratch = commit_with_files(
            &repo,
            &[
                ("a.txt", "a1"),
                (".github/ci.yml", "workflow"),
                ("scratch.txt", "manual"),
            ],
            "manual edit",
            Some(first.commit),
        );
        set_ref(&repo, "refs/heads/main", scratch);
        assert!(tree_has_entry(
            &repo,
            repo.find_commit(scratch).unwrap().tree_id().unwrap().detach(),
            "scratch.txt"
        ));

        // Recompose: because the artifact is rebuilt from upstream + stack,
        // the ad-hoc scratch.txt is wiped (it is not on any stack branch).
        let second = compose(
            &repo,
            "refs/heads/upstream/main",
            &branches,
            "refs/heads/main",
            sig(),
        )
        .expect("recompose");
        assert!(!tree_has_entry(&repo, second.tree, "scratch.txt"));
        assert_eq!(
            tree_blob(&repo, second.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(second.commit));
    }

    #[test]
    fn upstream_advance_does_not_become_branch_deletion() {
        let dir = temp_dir("upstream_advance");
        let repo = init_bare(&dir);

        // Upstream: c1 has a.txt, then advances to c2 adding b.txt — i.e. the
        // fork-owned branch is forked from the *older* c1 base.
        let c1 = commit_with_files(&repo, &[("a.txt", "a1")], "upstream c1", None);
        set_ref(&repo, "refs/heads/upstream/main", c1);
        let c2 = commit_with_files(
            &repo,
            &[("a.txt", "a1"), ("b.txt", "b2")],
            "upstream c2",
            Some(c1),
        );
        set_ref(&repo, "refs/heads/upstream/main", c2);

        // Fork-owned branch forked from c1: adds .github/ci.yml, does NOT have
        // b.txt (it predates c2).
        let owned = commit_with_files(
            &repo,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "owned",
            Some(c1),
        );
        set_ref(&repo, "refs/heads/fork-owned", owned);

        let branches = vec!["refs/heads/fork-owned".to_string()];
        let outcome = compose(
            &repo,
            "refs/heads/upstream/main", // mirror is already advanced to c2
            &branches,
            "refs/heads/main",
            sig(),
        )
        .expect("compose");

        // b.txt was added upstream after the fork; it is NOT a deletion the
        // branch made, so it survives. The fork-owned layer adds .github/ci.yml.
        assert_eq!(tree_blob(&repo, outcome.tree, "a.txt").as_deref(), Some("a1"));
        assert_eq!(tree_blob(&repo, outcome.tree, "b.txt").as_deref(), Some("b2"));
        assert_eq!(
            tree_blob(&repo, outcome.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
    }

    #[test]
    fn empty_stack_just_rebases_to_base_tree() {
        let dir = temp_dir("empty_stack");
        let repo = init_bare(&dir);

        let base = commit_with_files(&repo, &[("a.txt", "a1")], "base", None);
        set_ref(&repo, "refs/heads/upstream/main", base);

        let outcome = compose(
            &repo,
            "refs/heads/upstream/main",
            &[],
            "refs/heads/main",
            sig(),
        )
        .expect("compose empty stack");

        assert_eq!(outcome.patches_applied, 0);
        assert_eq!(tree_blob(&repo, outcome.tree, "a.txt").as_deref(), Some("a1"));
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(outcome.commit));
    }
}
