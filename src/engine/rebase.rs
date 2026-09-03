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
//!   │   Overlay   │ │ Cascade    │ │ (future)   │
//!   │  (current)  │ │ Rebase     │ │ Other      │
//!   └─────────────┘ └────────────┘ └────────────┘
//! ```
//!
//! The trait is object-safe and can be used as a dynamic dispatch in the
//! pipeline, or as a compile-time selection via generics.

use anyhow::Result;
use gix::Repository;
use gix::actor::SignatureRef;

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
             Use the Overlay strategy instead."
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
}
