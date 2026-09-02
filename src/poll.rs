//! Upstream-drift polling.
//!
//! The app is event-driven (webhooks) for *fork*-side changes, but it cannot
//! subscribe to the *upstream* repository (webhooks fire only for repos the
//! app is installed on — an install on the fork does not surface upstream
//! activity). Upstream drift on an otherwise idle fork is therefore only ever
//! observed by polling: periodically fetch the upstream branch, fast-forward
//! the mirror, and recompose the artifact.
//!
//! This module owns the *scheduling semantics*:
//! - iterate every configured fork,
//! - isolate per-fork failures (one bad fork must not abort the pass),
//! - classify each pass into a [`PollOutcome`] so callers can log whether
//!   anything actually changed (mirror advanced) or it was a no-op.
//!
//! The reconcile work itself is injected as an action (mirroring how the
//! webhook injects a handle), keeping this module pure, network-free, and
//! unit-testable.

use anyhow::{Context, Result};

use crate::config::ForkConfig;
use crate::engine::pipeline::{ReconcileOutcome, reconcile};
use crate::engine::sync::{FfOutcome, open_repo};

/// What happened to a single fork during one poll pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// The mirror was already at the upstream tip; nothing advanced.
    NoChange,
    /// Something changed: the mirror advanced and/or the artifact was
    /// recomposed. `note` describes what.
    Changed { note: String },
    /// The fork could not be reconciled (missing local mirror, git error).
    Failed(String),
}

/// Reconcile a single fork from its configuration.
///
/// Opens the fork's local mirror repo at `cfg.local_mirror`, then runs the
/// engine's [`reconcile`] pipeline against it (sync mirror + compose artifact).
/// `upstream_url` is the transport URL of the upstream repository (`file://` /
/// a local path in tests, `cfg.upstream.https_url()` in production).
///
/// `stack` is the ordered list of fork branch refs forming the artifact's
/// layers — in live mode this comes from open-PR discovery (a follow-up); it
/// is supplied explicitly here so the poll stays testable against local repos.
pub fn reconcile_fork(
    cfg: &ForkConfig,
    upstream_url: &str,
    stack: &[String],
    committer: gix::actor::SignatureRef<'_>,
) -> Result<ReconcileOutcome> {
    let path = cfg
        .local_mirror
        .as_deref()
        .context("fork has no local_mirror configured; cannot reconcile")?;
    let repo = open_repo(path)?;
    reconcile(&repo, cfg, upstream_url, stack, committer)
}

/// Run one poll pass across all `forks`.
///
/// Each fork's reconcile is independent: a failure for one fork is captured as
/// [`PollOutcome::Failed`] and the remaining forks still run. The returned
/// `Vec` is parallel to `forks`.
pub fn run_pass<F>(forks: &[ForkConfig], mut action: F) -> Vec<PollOutcome>
where
    F: FnMut(&ForkConfig) -> Result<ReconcileOutcome>,
{
    forks.iter().map(|fork| classify(action(fork))).collect()
}

/// Classify a reconcile result into a [`PollOutcome`].
///
/// The signal for "anything actually changed" is the mirror fast-forward state:
/// - the mirror advanced -> `Changed`;
/// - the mirror was already current -> `NoChange` (the recompose is idempotent
///   and produces the same artifact);
/// - the mirror is diverged -> we refuse to rewrite it (fast-forward-only), but
///   the artifact is still recomposed, so this is a `Changed` with a warning.
pub fn classify(result: Result<ReconcileOutcome>) -> PollOutcome {
    match result {
        Ok(outcome) => match outcome.sync.ff {
            FfOutcome::UpToDate => PollOutcome::NoChange,
            FfOutcome::FastForwarded { from, to } => PollOutcome::Changed {
                note: format!("mirror fast-forwarded {from} -> {to}"),
            },
            FfOutcome::NotFastForward { current, target } => PollOutcome::Changed {
                note: format!(
                    "mirror diverged (current {current}, upstream {target}); not rewritten, artifact recomposed"
                ),
            },
        },
        Err(e) => PollOutcome::Failed(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::pipeline::ReconcileOutcome;
    use crate::engine::stack::StackOutcome;
    use crate::engine::sync::SyncResult;
    use gix::ObjectId;
    use gix::actor::SignatureRef;
    use std::path::PathBuf;

    fn outcome_with_ff(ff: FfOutcome) -> ReconcileOutcome {
        ReconcileOutcome {
            sync: SyncResult {
                tip: ObjectId::null(gix::hash::Kind::Sha1),
                ff,
            },
            compose: StackOutcome {
                tree: ObjectId::null(gix::hash::Kind::Sha1),
                commit: ObjectId::null(gix::hash::Kind::Sha1),
                patches_applied: 0,
            },
        }
    }

    fn mkfork(fork_name: &str) -> ForkConfig {
        ForkConfig {
            upstream: crate::config::Repo {
                owner: "integrations".into(),
                name: "terraform-provider-github".into(),
            },
            fork: crate::config::Repo {
                owner: "myorg".into(),
                name: fork_name.into(),
            },
            default_branch: "main".into(),
            local_mirror: None,
            override_upstream_branch: None,
            fork_owned_branch: None,
        }
    }

    #[test]
    fn classifies_mirror_advanced_as_changed() {
        let outcome = outcome_with_ff(FfOutcome::FastForwarded {
            from: "a".into(),
            to: "b".into(),
        });
        let PollOutcome::Changed { note } = classify(Ok(outcome)) else {
            panic!("expected Changed");
        };
        assert!(note.contains("fast-forwarded"), "note: {note}");
    }

    #[test]
    fn classifies_up_to_date_as_no_change() {
        assert_eq!(
            classify(Ok(outcome_with_ff(FfOutcome::UpToDate))),
            PollOutcome::NoChange
        );
    }

    #[test]
    fn classifies_error_as_failed() {
        match classify(Err(anyhow::anyhow!("git exploded"))) {
            PollOutcome::Failed(msg) => assert!(msg.contains("git exploded")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classifies_diverged_as_changed_with_warning() {
        let outcome = outcome_with_ff(FfOutcome::NotFastForward {
            current: "c".into(),
            target: "u".into(),
        });
        let PollOutcome::Changed { note } = classify(Ok(outcome)) else {
            panic!("expected Changed");
        };
        assert!(note.contains("diverged"), "note: {note}");
    }

    #[test]
    fn run_pass_isolates_per_fork_failures() {
        let forks = vec![mkfork("a"), mkfork("b"), mkfork("c")];
        // The middle fork's action fails; the others still run.
        let outcomes = run_pass(&forks, |f| {
            if f.fork.name == "b" {
                return Err(anyhow::anyhow!("boom"));
            }
            Ok(outcome_with_ff(FfOutcome::UpToDate))
        });

        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0], PollOutcome::NoChange);
        assert!(matches!(&outcomes[1], PollOutcome::Failed(_)));
        assert_eq!(outcomes[2], PollOutcome::NoChange);
    }

    // --- reconcile_fork: end-to-end against a real local bare repo ---

    fn sig() -> SignatureRef<'static> {
        SignatureRef::from_bytes(b"tester <tester@example.com> 1711398853 +0000")
            .expect("valid sig")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn tree_blob(repo: &gix::Repository, tree_id: gix::ObjectId, path: &str) -> Option<String> {
        let mut tree = repo.find_tree(tree_id).expect("find tree");
        tree.peel_to_entry(path.split('/'))
            .expect("peel")
            .map(|entry| {
                let blob = repo.find_blob(entry.oid().to_owned()).expect("find blob");
                String::from_utf8_lossy(&blob.data).into_owned()
            })
    }

    fn ref_id(repo: &gix::Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    // A minimal upstream bare repo with one commit on `main`.
    fn upstream_with_one_commit(name: &str) -> (PathBuf, gix::Repository, gix::ObjectId) {
        use gix::objs::tree::EntryKind;
        use gix::refs::transaction::PreviousValue;
        let dir = temp_dir(name);
        let repo = gix::init_bare(&dir).expect("init upstream");
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        let blob = repo.write_blob("a1").expect("blob");
        editor
            .upsert("a.txt", EntryKind::Blob, blob.detach())
            .expect("upsert");
        let tree_id = editor.write().expect("write tree").detach();
        let commit = repo
            .new_commit_as(sig(), sig(), "upstream c1", tree_id, None::<gix::ObjectId>)
            .expect("commit")
            .id;
        repo.reference(
            "refs/heads/main",
            commit,
            PreviousValue::Any,
            "set ref for test",
        )
        .expect("set ref");
        (dir, repo, commit)
    }

    #[test]
    fn reconcile_fork_drives_mirror_and_artifact_from_local_mirror() {
        let (_up, _up_repo, c1) = upstream_with_one_commit("poll_up");

        // Fork mirror repo, empty bare.
        let fork_dir = temp_dir("poll_fork");
        let fork = gix::init_bare(&fork_dir).expect("init fork");

        // No stack layers; the base artifact alone should mirror upstream.
        let mut cfg = mkfork("myself");
        cfg.local_mirror = Some(fork_dir.display().to_string());

        let upstream_url = _up.display().to_string();
        let outcome = reconcile_fork(&cfg, &upstream_url, &[], sig()).expect("reconcile_fork");

        // Mirror advanced to the upstream tip; artifact carries upstream's tree.
        assert_eq!(ref_id(&fork, "refs/heads/upstream/main").unwrap(), c1);
        assert_eq!(
            tree_blob(&fork, outcome.compose.tree, "a.txt").as_deref(),
            Some("a1")
        );
        assert_eq!(
            ref_id(&fork, "refs/heads/main").unwrap(),
            outcome.compose.commit
        );
    }

    #[test]
    fn reconcile_fork_skips_missing_local_mirror() {
        let mut cfg = mkfork("norepo");
        cfg.local_mirror = None;
        let err = reconcile_fork(&cfg, "file:///nonexistent", &[], sig()).unwrap_err();
        assert!(
            err.to_string().contains("no local_mirror"),
            "unexpected: {err:#}"
        );
    }
}
