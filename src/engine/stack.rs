//! Patch stack overlay — the *stack* half of the composed artifact (milestone 4).
//!
//! The fork's artifact is **upstream base tree + patch stack + fork-owned
//! files**. The fork-owned overlay lives in [`compose`](super::compose); this
//! module layers the *patch stack* on top of a base.
//!
//! A *patch* is a branch whose changes belong in the fork's artifact. Patches
//! form a cascade: `P1` targets the upstream base, `P2` builds on `P1`, and so
//! on. To recompose the artifact, we take each patch's changes *relative to its
//! own base* and re-apply them, in order, onto the running composed tree.
//!
//! # Seam
//!
//! Which branches actually form the patch stack, and in what order, is decided
//! upstream of this module (discovered from open PRs by the GitHub layer, or
//! supplied explicitly). This module only needs an ordered list of refs and the
//! base ref; it is pure git and fully testable against local bare repositories.
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

    let mut changes = Vec::new();
    let mut platform = from_tree.changes()?;
    platform.options(|opts| {
        opts.track_rewrites(None);
    });
    platform.for_each_to_obtain_tree(&to_tree, |change| {
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

/// The outcome of composing a patch stack onto a base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackOutcome {
    /// The final composed tree id.
    pub tree: gix::ObjectId,
    /// The stack artifact commit id.
    pub commit: gix::ObjectId,
    /// The number of patches that were layered onto the base.
    pub patches_applied: usize,
}

/// Recompose a patch stack onto `base_ref` and point `target_ref` at the
/// result.
///
/// `patches` is an ordered list of refs forming the cascade. Each patch's
/// changes are computed relative to the *previous* layer —
/// `[base_ref] -> patches[0] -> patches[1] -> …` — and applied in sequence onto
/// the running composed tree. A new commit is written whose parent is `base_ref`
///'s commit, and `target_ref` is advanced to it.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if a patch ref or the base ref cannot be
/// resolved, or if the tree editor fails.
pub fn apply_patch_stack(
    repo: &Repository,
    base_ref: &str,
    patches: &[String],
    target_ref: &str,
    committer: gix::actor::SignatureRef<'_>,
) -> Result<StackOutcome> {
    let (base_tree_id, base_commit) = peel_tree_and_commit(repo, base_ref)?;
    let mut running = base_tree_id;

    // Each layer compares the previous ref's tree against the current patch's
    // tree, then re-applies those changes onto the running composed tree.
    let mut prev_ref = base_ref.to_string();
    for patch in patches {
        let changes = patch_changes(repo, &prev_ref, patch)?;
        running = apply_changes(repo, running, &changes)?;
        prev_ref = patch.clone();
    }

    let commit = repo
        .new_commit_as(
            committer,
            committer,
            format!(
                "Recompose artifact with patch stack ({} patches)",
                patches.len()
            ),
            running,
            [base_commit],
        )?
        .id;

    write_ref(repo, target_ref, commit)?;

    Ok(StackOutcome {
        tree: running,
        commit,
        patches_applied: patches.len(),
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
    fn layers_patch_stack_onto_base() {
        let dir = temp_dir("stack");
        let repo = init_bare(&dir);

        // Base: upstream mirror with a.txt.
        let base = commit_with_files(&repo, &[("a.txt", "a1")], "base", None);
        set_ref(&repo, "refs/heads/upstream/main", base);

        // P1 targets the base, touching b.txt (adds helper.txt).
        let p1 = commit_with_files(&repo, &[("a.txt", "a1"), ("helper.txt", "h")], "P1", Some(base));
        set_ref(&repo, "refs/heads/patch/1", p1);

        // P2 builds on P1, adding feature.txt and modifying a.txt.
        let p2 = commit_with_files(
            &repo,
            &[("a.txt", "a2"), ("helper.txt", "h"), ("feature.txt", "f")],
            "P2",
            Some(p1),
        );
        set_ref(&repo, "refs/heads/patch/2", p2);

        let patches = vec!["refs/heads/patch/1".to_string(), "refs/heads/patch/2".to_string()];
        let outcome = apply_patch_stack(
            &repo,
            "refs/heads/upstream/main",
            &patches,
            "refs/heads/main",
            sig(),
        )
        .expect("compose stack");

        assert_eq!(outcome.patches_applied, 2);
        // Final tree carries the cumulative patch changes.
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
    fn empty_stack_just_rebases_to_base_tree() {
        let dir = temp_dir("empty_stack");
        let repo = init_bare(&dir);

        let base = commit_with_files(&repo, &[("a.txt", "a1")], "base", None);
        set_ref(&repo, "refs/heads/upstream/main", base);

        let outcome = apply_patch_stack(
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
