//! Reconcile pipeline — drives the engine toward the fork's desired state.
//!
//! This is the high-level entry that composes the engine's two halves into a
//! single pass for a fork:
//!
//! 1. **Sync the mirror** (`upstream/<X>`): fetch the upstream branch and
//!    fast-forward the mirror ref (strict FF only — the mirror is never
//!    rewritten).
//! 2. **Compose the artifact** (`<X>`): reset the artifact to the upstream
//!    mirror tip, then layer the fork's ordered stack of branches on top.
//!
//! The stack is supplied explicitly — it is the *seam* where the GitHub layer
//! will later plug in open-PR discovery. For now it is pure git and fully
//! testable against local bare repositories.
//!
//! # Blocking
//!
//! This pass uses gix's blocking transport (fetch) and blocking object I/O.
//! In an async context, wrap the call in [`tokio::task::spawn_blocking`].

use anyhow::Result;
use gix::{Repository, actor::SignatureRef};

use crate::config::ForkConfig;
use crate::engine::stack::{StackOutcome, compose};
use crate::engine::sync::{SyncResult, sync_mirror};

/// The outcome of a full reconcile pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// How the upstream mirror was synced.
    pub sync: SyncResult,
    /// How the artifact was recomposed.
    pub compose: StackOutcome,
}

/// Reconcile a single fork toward its desired state.
///
/// Derived refs (see [`ForkConfig`]):
/// - mirror: `refs/heads/upstream/<X>`
/// - tracking (fetch): `refs/remotes/upstream/<X>`
/// - artifact: `refs/heads/<X>` where `<X>` is `fork_default_branch`
///   (or the override).
///
/// `upstream_url` is the transport URL of the upstream repository (HTTPS in
/// production, `file://`/local path in tests).
///
/// `stack` is the ordered list of fork branch refs forming the artifact's
/// layers — the fork-owned branch first, then the patch PRs. It is applied
/// on top of the freshly mirrored upstream tip.
pub fn reconcile(
    repo: &Repository,
    cfg: &ForkConfig,
    fork_default_branch: &str,
    upstream_url: &str,
    stack: &[String],
    committer: SignatureRef<'_>,
) -> Result<ReconcileOutcome> {
    let branch = cfg.upstream_branch(fork_default_branch);
    let mirror_ref = format!("refs/heads/{}", cfg.mirror_branch(fork_default_branch));
    let track_ref = format!("refs/remotes/{}", cfg.mirror_branch(fork_default_branch));
    let artifact_ref = format!("refs/heads/{fork_default_branch}");

    // 1. Sync the upstream mirror.
    let sync = sync_mirror(repo, upstream_url, &branch, &mirror_ref, &track_ref)?;

    // 2. Compose the artifact on top of the freshly synced mirror tip.
    let compose = compose(repo, &mirror_ref, stack, &artifact_ref, committer)?;

    Ok(ReconcileOutcome { sync, compose })
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

    fn cfg() -> ForkConfig {
        ForkConfig {
            upstream: crate::config::Repo {
                owner: "integrations".into(),
                name: "terraform-provider-github".into(),
            },
            fork: crate::config::Repo {
                owner: "myorg".into(),
                name: "terraform-provider-github".into(),
            },
            override_upstream_branch: None,
        }
    }

    #[test]
    fn reconcile_syncs_mirror_and_composes_artifact() {
        // Upstream repo: a.txt, then advances to add b-to-add upstream later.
        let upstream_dir = temp_dir("pipe_upstream");
        let upstream = init_bare(&upstream_dir);
        let c1 = commit_with_files(&upstream, &[("a.txt", "a1")], "upstream c1", None);
        set_ref(&upstream, "refs/heads/main", c1);

        // Fork: empty bare repo.
        let fork_dir = temp_dir("pipe_fork");
        let fork = init_bare(&fork_dir);

        // No stack branches configured yet.
        let stack: Vec<String> = vec![];
        let first = reconcile(
            &fork,
            &cfg(),
            "main",
            &upstream_dir.display().to_string(),
            &stack,
            sig(),
        )
        .expect("reconcile");

        // Mirror created and points at upstream tip; artifact is upstream with
        // no stack layers applied.
        assert_eq!(ref_id(&fork, "refs/heads/upstream/main"), Some(c1));
        assert_eq!(
            tree_blob(&fork, first.compose.tree, "a.txt").as_deref(),
            Some("a1")
        );
        assert_eq!(ref_id(&fork, "refs/heads/main"), Some(first.compose.commit));
    }

    #[test]
    fn reconcile_layers_configured_stack_after_fetch() {
        // Upstream advances from c1 (a.txt) to c2 (a.txt + b.txt).
        let upstream_dir = temp_dir("pipe_stack_up");
        let upstream = init_bare(&upstream_dir);
        let c1 = commit_with_files(&upstream, &[("a.txt", "a1")], "upstream c1", None);
        set_ref(&upstream, "refs/heads/main", c1);
        let c2 = commit_with_files(
            &upstream,
            &[("a.txt", "a1"), ("b.txt", "b2")],
            "upstream c2",
            Some(c1),
        );
        set_ref(&upstream, "refs/heads/main", c2);

        let fork_dir = temp_dir("pipe_stack_fork");
        let fork = init_bare(&fork_dir);

        // Fork-owned branch exists on the fork, carrying `.github/ci.yml`.
        let owned = commit_with_files(
            &fork,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "owned",
            Some(c1),
        );
        set_ref(&fork, "refs/heads/fork-owned", owned);

        let stack = vec!["refs/heads/fork-owned".to_string()];
        let outcome = reconcile(
            &fork,
            &cfg(),
            "main",
            &upstream_dir.display().to_string(),
            &stack,
            sig(),
        )
        .expect("reconcile");

        // Mirror reflects upstream tip c2.
        assert_eq!(outcome.sync.tip, c2);
        assert_eq!(ref_id(&fork, "refs/heads/upstream/main"), Some(c2));

        // Artifact adopts the new upstream content AND the fork-owned layer.
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, "a.txt").as_deref(),
            Some("a1")
        );
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, "b.txt").as_deref(),
            Some("b2")
        );
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
        assert_eq!(ref_id(&fork, "refs/heads/main"), Some(outcome.compose.commit));
    }

    #[test]
    fn reconcile_uses_override_upstream_branch() {
        let upstream_dir = temp_dir("pipe_override_up");
        let upstream = init_bare(&upstream_dir);
        let c1 = commit_with_files(&upstream, &[("a.txt", "a1")], "canonical c1", None);
        set_ref(&upstream, "refs/heads/canonical", c1);

        let fork_dir = temp_dir("pipe_override_fork");
        let fork = init_bare(&fork_dir);

        let mut cfg = cfg();
        cfg.override_upstream_branch = Some("canonical".into());
        let stack: Vec<String> = vec![];
        let outcome = reconcile(
            &fork,
            &cfg,
            "main",
            &upstream_dir.display().to_string(),
            &stack,
            sig(),
        )
        .expect("reconcile");

        // Mirror is upstream/canonical (the override), artifact is main.
        assert_eq!(ref_id(&fork, "refs/heads/upstream/canonical"), Some(c1));
        assert_eq!(ref_id(&fork, "refs/heads/main"), Some(outcome.compose.commit));
        assert!(!tree_has_entry(&fork, outcome.compose.tree, "nope.txt"));
    }

    #[test]
    fn reconcile_uses_discovered_pr_stack() {
        // The full seam: GitHub PR metadata -> discover_stack -> reconcile ->
        // a composed artifact. Open PR branches are made present locally as
        // refs/pull/<n>/head, mirroring how a deployment would fetch them.
        let upstream_dir = temp_dir("pipe_discover_up");
        let upstream = init_bare(&upstream_dir);
        let c1 = commit_with_files(&upstream, &[("a.txt", "a1")], "upstream c1", None);
        set_ref(&upstream, "refs/heads/main", c1);

        let fork_dir = temp_dir("pipe_discover_fork");
        let fork = init_bare(&fork_dir);

        // Fork-owned branch present locally.
        let owned = commit_with_files(
            &fork,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "owned",
            Some(c1),
        );
        set_ref(&fork, "refs/heads/fork-owned", owned);

        // Open PR 12 (feat-a) based on upstream main; fetched into its pull ref.
        let pr12 = commit_with_files(
            &fork,
            &[("a.txt", "a1"), ("feat.txt", "f")],
            "feat-a",
            Some(c1),
        );
        set_ref(&fork, "refs/pull/12/head", pr12);

        // Discovery: the fork's open PRs plus its fork-owned branch.
        let prs = vec![crate::github::PrInfo {
            number: 12,
            head_branch: "feat-a".into(),
            base_branch: "main".into(),
        }];
        let stack =
            crate::github::discover_stack("main", Some("fork-owned"), &prs).expect("discover");
        assert_eq!(
            stack,
            vec![
                "refs/heads/fork-owned".to_string(),
                "refs/pull/12/head".to_string()
            ]
        );

        let outcome = reconcile(
            &fork,
            &cfg(),
            "main",
            &upstream_dir.display().to_string(),
            &stack,
            sig(),
        )
        .expect("reconcile");

        // Artifact carries the fork-owned layer AND the PR's change.
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, "feat.txt").as_deref(),
            Some("f")
        );
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, "a.txt").as_deref(),
            Some("a1")
        );
        assert_eq!(ref_id(&fork, "refs/heads/main"), Some(outcome.compose.commit));
    }
}
