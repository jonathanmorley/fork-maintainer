//! Cascade-rebase strategy — trait abstraction for rebasing a branch stack.
//!
//! The current composition strategy (`engine::stack::compose`) is a **tree
//! overlay**: each branch's path-level changes are applied on top of the
//! upstream base in order, with last-write-wins semantics. This is fast and
//! deterministic but does not detect or resolve conflicts.
//!
//! A **cascade-rebase** would rebase each branch in the stack onto the
//! upstream mirror tip, producing a clean linear history and detecting
//! conflicts at each layer. This is the desired behavior for production use
//! but requires a rebase implementation, which gix 0.87.1 does not expose in
//! its public API (it is "idea" stage).
//!
//! This module defines the [`Rebase`] trait so that:
//! 1. The current overlay strategy can serve as the default implementation.
//! 2. When gix adds rebase support (or a gix-based rebase is implemented),
//!    it can be swapped in without changing the pipeline or reconcile logic.
//!
//! # Design
//!
//! ```text
//!                   ┌──────────────┐
//!                   │  Rebase trait │
//!                   └──────┬───────┘
//!                          │
//!          ┌───────────────┼───────────────┐
//!          │               │               │
//!   ┌──────▼──────┐ ┌─────▼──────┐ ┌─────▼──────┐
//!   │   Overlay   │ │   Merge    │ │ Cascade    │
//!   │  (current)  │ │ (3-way)    │ │ Rebase     │
//!   └─────────────┘ └────────────┘ └────────────┘
//! ```
//!
//! The trait is object-safe and can be used as a dynamic dispatch in the
//! pipeline, or as a compile-time selection via generics.

use anyhow::{Context, Result};
use gix::Repository;
use gix::actor::SignatureRef;
use gix::diff::tree_with_rewrites::Change as TreeChange;

/// The result of composing a branch stack onto a base.
///
/// This is the same struct as [`crate::engine::stack::StackOutcome`] but
/// lives here to avoid circular dependencies. The pipeline uses whichever
/// strategy is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeOutcome {
    /// The final composed tree id.
    pub tree: gix::ObjectId,
    /// The stack artifact commit id.
    pub commit: gix::ObjectId,
    /// The number of stack branches that were layered onto the base.
    pub patches_applied: usize,
}

/// Strategy for composing a branch stack onto an upstream base.
///
/// Implementors produce a new commit whose tree reflects the upstream base
/// with the stack's branches applied in order. The current default is
/// [`Overlay`]; a cascade-rebase implementation will replace it when gix
/// adds rebase support.
pub trait Rebase {
    /// Compose the artifact from an ordered stack of fork branches layered
    /// on `base_ref`.
    ///
    /// `branches` is the ordered list of refs forming the cascade — the
    /// fork-owned branch first, then the patch PRs. `target_ref` is the
    /// local ref to advance (e.g. `refs/heads/main`).
    fn compose(
        &self,
        repo: &Repository,
        base_ref: &str,
        branches: &[String],
        target_ref: &str,
        committer: SignatureRef<'_>,
    ) -> Result<ComposeOutcome>;
}

/// Tree overlay strategy — the current default composition approach.
///
/// Each branch's path-level changes are computed against its own fork point
/// and applied to the running tree in order. Last-write-wins; no conflict
/// detection.
pub struct Overlay;

impl Rebase for Overlay {
    fn compose(
        &self,
        repo: &Repository,
        base_ref: &str,
        branches: &[String],
        target_ref: &str,
        committer: SignatureRef<'_>,
    ) -> Result<ComposeOutcome> {
        let outcome =
            crate::engine::stack::compose(repo, base_ref, branches, target_ref, committer)?;
        Ok(ComposeOutcome {
            tree: outcome.tree,
            commit: outcome.commit,
            patches_applied: outcome.patches_applied,
        })
    }
}

/// Three-way merge strategy — uses gix's `merge_trees` for conflict detection.
///
/// For each branch in the stack, computes the3-way merge between:
/// - **ancestor**: the branch's own fork point (first parent)
/// - **ours**: the running composed tree
/// - **theirs**: the branch's head tree
///
/// This detects conflicts at each layer. Auto-resolved conflicts (e.g.
/// non-overlapping changes) are applied; true conflicts cause the compose
/// to fail with an error listing the conflicted paths.
///
/// This is a stepping stone toward full cascade-rebase: it provides
/// conflict detection without the linear-history rebase semantics.
pub struct Merge;

impl Rebase for Merge {
    fn compose(
        &self,
        repo: &Repository,
        base_ref: &str,
        branches: &[String],
        target_ref: &str,
        committer: SignatureRef<'_>,
    ) -> Result<ComposeOutcome> {
        // Start with the upstream base tree.
        let base_oid = repo.find_reference(base_ref)?.id().detach();
        let base_commit = repo.find_commit(base_oid)?;
        let mut running_tree = base_commit.tree_id()?.detach();

        for branch in branches {
            let branch_oid = repo.find_reference(branch)?.id().detach();
            let branch_commit = repo.find_commit(branch_oid)?;
            let branch_tree = branch_commit.tree_id()?.detach();

            // The patch's fork point — its merge-base with the base tip — is
            // the ancestor. This captures the patch's entire unique content
            // (all of its commits), unlike an immediate parent which would
            // only capture the last commit on multi-commit branches.
            let ancestor_tree = crate::engine::stack::fork_point_tree(repo, branch_oid, base_oid)?;

            // Perform 3-way merge: ours=running, theirs=branch, ancestor=branch's fork point.
            let labels = gix::merge::blob::builtin_driver::text::Labels {
                ancestor: Some(branch.as_bytes().into()),
                current: Some(b"running-composed".as_ref().into()),
                other: Some(branch.as_bytes().into()),
            };

            let options = repo
                .tree_merge_options()?
                // No rename/copy tracking: layers are independently authored
                // patch branches, so cross-file similarity is boilerplate
                // coincidence, never intent. Rename pairing actively harms
                // layering — observed live when two 90%-similar example files
                // (one per patch) were paired into a spurious conflict. The
                // overlay path likewise diffs with rewrites disabled.
                .with_rewrites(None);
            let mut outcome = repo
                .merge_trees(ancestor_tree, running_tree, branch_tree, labels, options)
                .with_context(|| format!("3-way merge for branch `{branch}`"))?;

            // Judge conflicts the way git would (shared helper: identical
            // additions are agreed content, the rest must be unresolvable
            // to fail the run).
            settle_conflicts(&mut outcome, branch)?;

            // Write the merged tree and advance the running tree.
            let merged_tree_id = outcome.tree.write()?.detach();
            running_tree = merged_tree_id;
        }

        // Create the commit and advance target_ref.
        let commit = repo
            .new_commit_as(
                committer,
                committer,
                format!(
                    "Recompose artifact from {} stack branches (merge strategy)",
                    branches.len()
                ),
                running_tree,
                [base_oid],
            )?
            .id;

        // Write the target ref.
        use gix::refs::Target;
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        repo.edit_reference(RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: format!("fork-maintainer: recompose stack artifact to {commit}")
                        .into(),
                },
                expected: PreviousValue::Any,
                new: Target::Object(commit),
            },
            name: gix::refs::FullName::try_from(target_ref)?,
            deref: false,
        })?;

        Ok(ComposeOutcome {
            tree: running_tree,
            commit,
            patches_applied: branches.len(),
        })
    }
}

/// Settle a merge outcome's conflicts the way git would.
///
/// gix-merge lists even auto-resolved entries in `conflicts`, so an
/// emptiness check would cry wolf — judge by `is_unresolved` with
/// git-compatible strictness instead. Identical additions on both sides
/// (same path, mode, and blob — routine when an upper patch contains a
/// lower patch's files verbatim) are agreed content: take them into the
/// outcome tree. gix-merge conservatively reports those as `Err(Unknown)`
/// while git merges them cleanly.
///
/// `layer` names the patch (and commit, for replays) for error messages.
/// On unresolved conflicts this bails listing the paths; the caller must
/// treat that as fatal before anything is pushed.
pub(crate) fn settle_conflicts(
    outcome: &mut gix::merge::tree::Outcome<'_>,
    layer: &str,
) -> Result<()> {
    let mut actionable: Vec<String> = Vec::new();
    for conflict in &outcome.conflicts {
        if let (
            TreeChange::Addition {
                location: ours_loc,
                id: ours_id,
                entry_mode: ours_mode,
                ..
            },
            TreeChange::Addition {
                location: theirs_loc,
                id: theirs_id,
                entry_mode: theirs_mode,
                ..
            },
        ) = (&conflict.ours, &conflict.theirs)
            && ours_loc == theirs_loc
            && ours_id == theirs_id
            && ours_mode == theirs_mode
        {
            outcome.tree.upsert(ours_loc, ours_mode.kind(), *ours_id)?;
        } else if conflict.is_unresolved(gix::merge::tree::TreatAsUnresolved::git()) {
            actionable.push(conflict.ours.location().to_string());
        }
    }
    if !actionable.is_empty() {
        actionable.sort();
        actionable.dedup();
        anyhow::bail!(
            "conflicts detected while merging patch `{layer}` at {} path(s): {}. \
             Resolve on the patch branch (never on the synthesized output) and re-run; \
             nothing was pushed",
            actionable.len(),
            actionable.join(", ")
        );
    }
    Ok(())
}

/// Cascade-rebase strategy — placeholder for future gix rebase support.
///
/// This struct exists to document the intended design and provide a
/// compile-time placeholder. Calling `compose` on it will return an error
/// until a rebase implementation is available.
///
/// When gix adds rebase support, this will:
/// 1. Rebase each branch in `branches` onto the upstream mirror tip.
/// 2. Detect conflicts at each layer.
/// 3. Produce a clean linear history with proper parent chains.
pub struct CascadeRebase;

impl Rebase for CascadeRebase {
    fn compose(
        &self,
        _repo: &Repository,
        _base_ref: &str,
        _branches: &[String],
        _target_ref: &str,
        _committer: SignatureRef<'_>,
    ) -> Result<ComposeOutcome> {
        anyhow::bail!(
            "cascade-rebase is not yet implemented: gix 0.87.1 has no rebase API. \
             Use the Overlay or Merge strategy instead."
        )
    }
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("rebase-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn commit_with_file(
        repo: &gix::Repository,
        name: &str,
        content: &str,
        message: &str,
        parent: Option<gix::ObjectId>,
    ) -> gix::ObjectId {
        let blob = repo.write_blob(content).expect("write blob");
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        editor
            .upsert(name, EntryKind::Blob, blob.detach())
            .expect("upsert");
        let tree_id = editor.write().expect("write tree").detach();
        repo.new_commit_as(sig(), sig(), message, tree_id, parent)
            .expect("new commit")
            .id
    }

    fn ref_id(repo: &gix::Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    /// Overlay strategy composes a stack correctly.
    #[test]
    fn overlay_composes_stack() {
        let dir = temp_dir("overlay");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with_file(&repo, "a.txt", "a1", "base", None);
        repo.reference("refs/heads/upstream/main", base, PreviousValue::Any, "init")
            .expect("set base");

        let overlay = commit_with_file(&repo, ".github/ci.yml", "workflow", "overlay", Some(base));
        repo.reference(
            "refs/heads/fork-owned",
            overlay,
            PreviousValue::Any,
            "init overlay",
        )
        .expect("set overlay");

        let strategy = Overlay;
        let outcome = strategy
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &["refs/heads/fork-owned".to_string()],
                "refs/heads/main",
                sig(),
            )
            .expect("compose");

        assert_eq!(outcome.patches_applied, 1);
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(outcome.commit));
    }

    /// Merge strategy composes non-conflicting branches.
    #[test]
    fn merge_composes_non_conflicting() {
        let dir = temp_dir("merge_nonconflict");
        let repo = gix::init_bare(&dir).expect("init bare");

        // Base: a.txt
        let base = commit_with_file(&repo, "a.txt", "a1", "base", None);
        repo.reference("refs/heads/upstream/main", base, PreviousValue::Any, "init")
            .expect("set base");

        // Branch adds .github/ci.yml (non-overlapping with base).
        let branch = commit_with_file(&repo, ".github/ci.yml", "workflow", "add ci", Some(base));
        repo.reference(
            "refs/heads/fork-owned",
            branch,
            PreviousValue::Any,
            "init branch",
        )
        .expect("set branch");

        let strategy = Merge;
        let outcome = strategy
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &["refs/heads/fork-owned".to_string()],
                "refs/heads/main",
                sig(),
            )
            .expect("merge compose");

        assert_eq!(outcome.patches_applied, 1);
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(outcome.commit));
    }

    /// Merge strategy detects conflicts.
    #[test]
    fn merge_detects_conflicts() {
        let dir = temp_dir("merge_conflict");
        let repo = gix::init_bare(&dir).expect("init bare");

        // Base: a.txt = "a1"
        let base = commit_with_file(&repo, "a.txt", "a1", "base", None);
        repo.reference("refs/heads/upstream/main", base, PreviousValue::Any, "init")
            .expect("set base");

        // Branch modifies a.txt (conflicts with upstream if upstream also changed it).
        // For 3-way merge: ancestor=base, ours=base, theirs=branch.
        // Since ours didn't change a.txt but theirs did, this is NOT a conflict.
        // We need a real conflict: both sides change the same file differently.
        let branch = commit_with_file(&repo, "a.txt", "a2", "modify a", Some(base));
        repo.reference(
            "refs/heads/feature",
            branch,
            PreviousValue::Any,
            "init feature",
        )
        .expect("set feature");

        // Now advance the running tree to also modify a.txt differently.
        // We'll compose feature first (non-conflicting), then try another
        // branch that conflicts with the composed result.
        let strategy_merge = Merge;
        let _first = strategy_merge
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &["refs/heads/feature".to_string()],
                "refs/heads/main",
                sig(),
            )
            .expect("first compose");
        // At this point refs/heads/main points at the composed commit with a.txt="a2".

        // Now create a branch that also modifies a.txt to a3, forked from base.
        let conflicting = commit_with_file(&repo, "a.txt", "a3", "conflict", Some(base));
        repo.reference(
            "refs/heads/conflict-branch",
            conflicting,
            PreviousValue::Any,
            "init conflict",
        )
        .expect("set conflict branch");

        // The merge strategy will try to merge conflict-branch (a.txt->a3)
        // into the running tree (a.txt->a2), using the fork point (a.txt->a1)
        // as ancestor. Both sides changed a.txt differently => conflict.
        let err = strategy_merge
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &[
                    "refs/heads/feature".to_string(),
                    "refs/heads/conflict-branch".to_string(),
                ],
                "refs/heads/main",
                sig(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("conflicts detected"),
            "expected conflict error, got: {err}"
        );
        assert!(
            err.to_string().contains("a.txt"),
            "expected conflicted path in error, got: {err}"
        );
        assert!(
            err.to_string().contains("conflict-branch"),
            "expected patch layer in error, got: {err}"
        );
    }

    /// CascadeRebase returns an error (not yet implemented).
    #[test]
    fn cascade_rebase_returns_error() {
        let dir = temp_dir("cascade_err");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with_file(&repo, "a.txt", "a1", "base", None);
        repo.reference("refs/heads/upstream/main", base, PreviousValue::Any, "init")
            .expect("set base");

        let strategy = CascadeRebase;
        let err = strategy
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &[],
                "refs/heads/main",
                sig(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("not yet implemented"),
            "unexpected error: {err}"
        );
    }

    /// Merge strategy with multiple non-conflicting branches.
    #[test]
    fn merge_composes_multiple_branches() {
        let dir = temp_dir("merge_multi");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with_file(&repo, "a.txt", "a1", "base", None);
        repo.reference("refs/heads/upstream/main", base, PreviousValue::Any, "init")
            .expect("set base");

        // Branch 1: adds b.txt
        let b1 = commit_with_file(&repo, "b.txt", "b1", "add b", Some(base));
        repo.reference("refs/heads/b1", b1, PreviousValue::Any, "init b1")
            .expect("set b1");

        // Branch 2: adds c.txt (forked from base, independent of b1)
        let b2 = commit_with_file(&repo, "c.txt", "c1", "add c", Some(base));
        repo.reference("refs/heads/b2", b2, PreviousValue::Any, "init b2")
            .expect("set b2");

        let strategy = Merge;
        let outcome = strategy
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &["refs/heads/b1".to_string(), "refs/heads/b2".to_string()],
                "refs/heads/main",
                sig(),
            )
            .expect("merge compose");

        assert_eq!(outcome.patches_applied, 2);
        assert_eq!(ref_id(&repo, "refs/heads/main"), Some(outcome.commit));
    }

    /// An auto-resolved content merge must not fail the run.
    ///
    /// Blobs extracted from a live failure (provider.go across three stacked
    /// states: ancestor, layer result, stacked tip). gix lists the
    /// successfully merged content in `conflicts`; judging by emptiness
    /// fails this clean merge, while `is_unresolved(git())` passes it —
    /// matching git, which merges the same inputs without conflict.
    #[test]
    fn auto_resolved_blob_merge_does_not_fail() {
        let dir = temp_dir("auto_resolved");
        let repo = gix::init_bare(&dir).expect("init bare");

        let commit_provider = |content: &str, parent: Option<gix::ObjectId>| {
            let blob = repo.write_blob(content).expect("write blob");
            let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
            editor
                .upsert("provider.go", EntryKind::Blob, blob.detach())
                .expect("upsert");
            let tree_id = editor.write().expect("write tree").detach();
            repo.new_commit_as(sig(), sig(), "m", tree_id, parent)
                .expect("new commit")
                .id
        };

        let v0 = include_str!("fixtures/provider_v0.go");
        let v1 = include_str!("fixtures/provider_v1.go");
        let v2 = include_str!("fixtures/provider_v2.go");

        let base = commit_provider(v0, None);
        repo.reference("refs/heads/upstream/main", base, PreviousValue::Any, "init")
            .expect("set base");
        let l1 = commit_provider(v1, Some(base));
        repo.reference("refs/heads/l1", l1, PreviousValue::Any, "init l1")
            .expect("set l1");
        let l2 = commit_provider(v2, Some(l1));
        repo.reference("refs/heads/l2", l2, PreviousValue::Any, "init l2")
            .expect("set l2");

        let outcome = Merge
            .compose(
                &repo,
                "refs/heads/upstream/main",
                &["refs/heads/l1".to_string(), "refs/heads/l2".to_string()],
                "refs/heads/main",
                sig(),
            )
            .expect("auto-resolved merge must succeed");

        // The merged tree carries the stacked tip's content.
        let mut tree = repo.find_tree(outcome.tree).expect("find tree");
        let entry = tree
            .peel_to_entry(["provider.go"])
            .expect("peel")
            .expect("provider.go present");
        let blob = repo.find_blob(entry.oid().to_owned()).expect("find blob");
        assert_eq!(String::from_utf8_lossy(&blob.data).as_ref(), v2);
    }
}
