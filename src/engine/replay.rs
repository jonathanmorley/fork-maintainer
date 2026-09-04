//! Commit-preserving replay — cherry-pick each patch's unique commits.
//!
//! Instead of squashing a patch layer into one commit ([`crate::engine::stack`]
//! / [`crate::engine::rebase`]), replay re-applies every commit unique to the
//! patch (fork-point..tip, first-parent walk) onto the running head. Each
//! replayed commit keeps its original message and author, plus a
//! `Synthesized-from:` trailer pointing at the original SHA; the committer
//! is the synthesis identity.
//!
//! Rules:
//! - A patch whose tip is already an ancestor of the base (merged upstream)
//!   contributes nothing and is skipped.
//! - No-op commits (tree identical to parent) are skipped, never replayed
//!   as empty commits.
//! - Merge commits in range are rejected: replay is linear-only.
//! - A conflict fails the run before the target ref moves, naming the patch,
//!   the commit, and the paths — nothing is pushed.

use anyhow::{Context, Result};
use gix::{Repository, actor::SignatureRef};

use crate::engine::rebase::settle_conflicts;

/// Outcome of a replay pass over all patch layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// The final head commit id.
    pub head: gix::ObjectId,
    /// The final composed tree id.
    pub tree: gix::ObjectId,
    /// Commits replayed across all layers.
    pub commits_replayed: usize,
    /// Commits skipped (already-upstream layers, no-op commits).
    pub skipped: usize,
}

/// Commits in `tip` not reachable from `base_oid`, oldest-first.
///
/// Walks the first-parent chain from `tip` down to (excluding) the merge-base
/// with `base_oid`. A missing merge-base (disjoint histories) means the whole
/// chain to the root is unique.
///
/// # Errors
///
/// Returns an error when a commit in range has more than one parent
/// (replay is linear-only).
fn unique_commits(
    repo: &Repository,
    tip: gix::ObjectId,
    base_oid: gix::ObjectId,
) -> Result<Vec<gix::ObjectId>> {
    let fork_point = match repo.merge_base(tip, base_oid) {
        Ok(mb) => Some(mb.detach()),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => None,
        Err(e) => return Err(e.into()),
    };
    if fork_point == Some(tip) {
        return Ok(vec![]);
    }
    let mut unique = vec![];
    let mut cur = tip;
    loop {
        if Some(cur) == fork_point {
            break;
        }
        let commit = repo.find_commit(cur)?;
        if commit.parent_ids().count() > 1 {
            anyhow::bail!(
                "cannot replay merge commit {cur}: replay is linear-only; \
                 linearize the patch branch first"
            );
        }
        unique.push(cur);
        match commit.parent_ids().next() {
            Some(parent) => cur = parent.detach(),
            None => break,
        }
    }
    unique.reverse();
    Ok(unique)
}

/// Replay one commit onto the running head.
///
/// Returns the new head, or `None` when the commit is a no-op (tree identical
/// to its parent) and must be skipped.
fn replay_one(
    repo: &Repository,
    running_oid: gix::ObjectId,
    running_tree: gix::ObjectId,
    original: gix::ObjectId,
    layer: &str,
    committer: SignatureRef<'_>,
) -> Result<Option<(gix::ObjectId, gix::ObjectId)>> {
    let commit = repo.find_commit(original)?;
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => repo.find_commit(parent)?.tree_id()?.detach(),
        None => repo.empty_tree().id,
    };
    let commit_tree = commit.tree_id()?.detach();
    if parent_tree == commit_tree {
        return Ok(None);
    }

    let other_label = format!("{original}");
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some(layer.as_bytes().into()),
        current: Some(b"running-replayed".as_ref().into()),
        other: Some(other_label.as_bytes().into()),
    };
    let options = repo.tree_merge_options()?.with_rewrites(None);
    let mut outcome = repo
        .merge_trees(parent_tree, running_tree, commit_tree, labels, options)
        .with_context(|| format!("replay commit {original} from `{layer}`"))?;
    settle_conflicts(&mut outcome, &format!("{layer}@{original}"))?;
    let merged_tree = outcome.tree.write()?.detach();

    let author = commit.author()?;
    let author = format!("{} <{}> {}", author.name, author.email, author.time);
    let author_sig =
        SignatureRef::from_bytes(author.as_bytes()).context("round-trip original author")?;
    let message = format!(
        "{}\n\nSynthesized-from: {original}",
        commit.message_raw()?.to_string().trim_end()
    );
    let new_head = repo
        .new_commit_as(committer, author_sig, message, merged_tree, [running_oid])?
        .id;
    Ok(Some((new_head, merged_tree)))
}

/// Replay every patch layer's unique commits onto the base.
///
/// `base_ref` is the freshly fetched base; `branches` are ordered local refs.
/// `target_ref` advances to the final head. Pure git against the local repo.
pub fn replay(
    repo: &Repository,
    base_ref: &str,
    branches: &[String],
    target_ref: &str,
    committer: SignatureRef<'_>,
) -> Result<ReplayOutcome> {
    let base_oid = repo.find_reference(base_ref)?.id().detach();
    let base_commit = repo.find_commit(base_oid)?;
    let mut running_oid = base_oid;
    let mut running_tree = base_commit.tree_id()?.detach();
    let mut commits_replayed = 0usize;
    let mut skipped = 0usize;

    for branch in branches {
        let tip = repo.find_reference(branch)?.id().detach();
        let unique = unique_commits(repo, tip, base_oid)?;
        if unique.is_empty() {
            tracing::info!(branch = %branch, "patch already upstream; skipping layer");
            skipped += 1;
            continue;
        }
        for original in unique {
            match replay_one(repo, running_oid, running_tree, original, branch, committer)? {
                Some((head, tree)) => {
                    running_oid = head;
                    running_tree = tree;
                    commits_replayed += 1;
                }
                None => {
                    tracing::info!(commit = %original, branch = %branch, "skipping no-op commit");
                    skipped += 1;
                }
            }
        }
    }

    repo.edit_reference(gix::refs::transaction::RefEdit {
        change: gix::refs::transaction::Change::Update {
            log: gix::refs::transaction::LogChange {
                mode: gix::refs::transaction::RefLog::AndReference,
                force_create_reflog: false,
                message: format!("synthesize: replay stack to {running_oid}").into(),
            },
            expected: gix::refs::transaction::PreviousValue::Any,
            new: gix::refs::Target::Object(running_oid),
        },
        name: gix::refs::FullName::try_from(target_ref)?,
        deref: false,
    })?;

    Ok(ReplayOutcome {
        head: running_oid,
        tree: running_tree,
        commits_replayed,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;
    use gix::refs::transaction::PreviousValue;
    use std::path::PathBuf;

    const SIG: &[u8] = b"tester <tester@example.com> 1711398853 +0000";
    const ALICE: &[u8] = b"alice <alice@example.com> 1711398854 +0000";

    fn sig() -> SignatureRef<'static> {
        SignatureRef::from_bytes(SIG).expect("valid sig")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn commit_with(
        repo: &Repository,
        files: &[(&str, &str)],
        message: &str,
        author: &[u8],
        parent: Option<gix::ObjectId>,
    ) -> gix::ObjectId {
        let author_sig = SignatureRef::from_bytes(author).expect("author sig");
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        for (name, content) in files {
            let blob = repo.write_blob(content).expect("write blob");
            editor
                .upsert(*name, EntryKind::Blob, blob.detach())
                .expect("upsert");
        }
        let tree_id = editor.write().expect("write tree").detach();
        repo.new_commit_as(sig(), author_sig, message, tree_id, parent)
            .expect("new commit")
            .id
    }

    fn set_ref(repo: &Repository, name: &str, target: gix::ObjectId) {
        repo.reference(name, target, PreviousValue::Any, "set ref for test")
            .expect("set ref");
    }

    fn ref_id(repo: &Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
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

    fn commit_message(repo: &Repository, oid: gix::ObjectId) -> String {
        repo.find_commit(oid)
            .expect("find commit")
            .message_raw()
            .expect("message")
            .to_string()
    }

    fn commit_author(repo: &Repository, oid: gix::ObjectId) -> String {
        let commit = repo.find_commit(oid).expect("find commit");
        let author = commit.author().expect("author");
        format!("{} <{}> {}", author.name, author.email, author.time)
    }

    /// Each unique commit replays with its message and author, plus trailer.
    #[test]
    fn replays_unique_commits_with_provenance() {
        let dir = temp_dir("provenance");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with(&repo, &[("a.txt", "a1")], "base", SIG, None);
        set_ref(&repo, "refs/heads/base", base);

        let c1 = commit_with(
            &repo,
            &[("a.txt", "a1"), ("f.txt", "f")],
            "add feature",
            ALICE,
            Some(base),
        );
        let c2 = commit_with(
            &repo,
            &[("a.txt", "a2"), ("f.txt", "f")],
            "tweak a",
            ALICE,
            Some(c1),
        );
        set_ref(&repo, "refs/heads/feature", c2);

        let out = replay(
            &repo,
            "refs/heads/base",
            &["refs/heads/feature".into()],
            "refs/heads/out",
            sig(),
        )
        .expect("replay");

        assert_eq!(out.commits_replayed, 2);
        assert_eq!(out.skipped, 0);
        assert_eq!(ref_id(&repo, "refs/heads/out"), Some(out.head));

        // History is base -> replay(c1) -> replay(c2), original messages kept.
        let head = repo.find_commit(out.head).expect("head");
        let parent = head.parent_ids().next().expect("parent").detach();
        assert!(commit_message(&repo, out.head).contains("tweak a"));
        assert!(commit_message(&repo, out.head).contains(&format!("Synthesized-from: {c2}")));
        assert!(commit_message(&repo, parent).contains("add feature"));
        assert!(commit_message(&repo, parent).contains(&format!("Synthesized-from: {c1}")));
        assert_eq!(
            commit_author(&repo, out.head).trim(),
            "alice <alice@example.com> 1711398854 +0000"
        );

        // Content landed fully.
        assert_eq!(tree_blob(&repo, out.tree, "f.txt").as_deref(), Some("f"));
        assert_eq!(tree_blob(&repo, out.tree, "a.txt").as_deref(), Some("a2"));
    }

    /// No-op commits are skipped, never replayed empty.
    #[test]
    fn skips_empty_commits() {
        let dir = temp_dir("empty");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with(&repo, &[("a.txt", "a1")], "base", SIG, None);
        set_ref(&repo, "refs/heads/base", base);
        // Same tree as parent: no-op.
        let noop = commit_with(&repo, &[("a.txt", "a1")], "noop", SIG, Some(base));
        let real = commit_with(&repo, &[("a.txt", "a2")], "real", SIG, Some(noop));
        set_ref(&repo, "refs/heads/feature", real);

        let out = replay(
            &repo,
            "refs/heads/base",
            &["refs/heads/feature".into()],
            "refs/heads/out",
            sig(),
        )
        .expect("replay");

        assert_eq!(out.commits_replayed, 1);
        assert_eq!(out.skipped, 1);
        assert_eq!(tree_blob(&repo, out.tree, "a.txt").as_deref(), Some("a2"));
    }

    /// A patch already contained in the base contributes nothing.
    #[test]
    fn skips_already_upstream_layer() {
        let dir = temp_dir("upstream");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with(&repo, &[("a.txt", "a1")], "base", SIG, None);
        let tip = commit_with(&repo, &[("a.txt", "a2")], "tip", SIG, Some(base));
        set_ref(&repo, "refs/heads/base", tip);
        // Feature tip is an ancestor of base: nothing unique.
        set_ref(&repo, "refs/heads/feature", base);

        let out = replay(
            &repo,
            "refs/heads/base",
            &["refs/heads/feature".into()],
            "refs/heads/out",
            sig(),
        )
        .expect("replay");

        assert_eq!(out.commits_replayed, 0);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.head, tip);
    }

    /// Merge commits in range are rejected with a clear error.
    #[test]
    fn rejects_merge_commits() {
        let dir = temp_dir("merges");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with(&repo, &[("a.txt", "a1")], "base", SIG, None);
        set_ref(&repo, "refs/heads/base", base);
        let left = commit_with(
            &repo,
            &[("a.txt", "a1"), ("l.txt", "l")],
            "left",
            SIG,
            Some(base),
        );
        let right = commit_with(
            &repo,
            &[("a.txt", "a1"), ("r.txt", "r")],
            "right",
            SIG,
            Some(base),
        );
        // Merge commit with two parents.
        let blob = repo.write_blob("a1").expect("blob");
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        editor
            .upsert("a.txt", EntryKind::Blob, blob.detach())
            .expect("upsert");
        let tree_id = editor.write().expect("write tree").detach();
        let merge = repo
            .new_commit_as(sig(), sig(), "merge", tree_id, [left, right])
            .expect("merge commit")
            .id;
        set_ref(&repo, "refs/heads/feature", merge);

        let err = replay(
            &repo,
            "refs/heads/base",
            &["refs/heads/feature".into()],
            "refs/heads/out",
            sig(),
        )
        .expect_err("merge commit must fail");
        assert!(err.to_string().contains("linear-only"), "got: {err}");
        assert_eq!(ref_id(&repo, "refs/heads/out"), None);
    }

    /// A conflicting replayed commit fails naming patch, commit, and paths.
    #[test]
    fn conflict_names_patch_commit_and_paths() {
        let dir = temp_dir("conflict");
        let repo = gix::init_bare(&dir).expect("init bare");

        let base = commit_with(&repo, &[("a.txt", "a1")], "base", SIG, None);
        set_ref(&repo, "refs/heads/base", base);
        // Layer 1 moves a.txt to a2.
        let l1 = commit_with(&repo, &[("a.txt", "a2")], "layer one", SIG, Some(base));
        set_ref(&repo, "refs/heads/layer1", l1);
        // Layer 2 (forked from base) moves a.txt to a3: genuine overlap.
        let l2 = commit_with(&repo, &[("a.txt", "a3")], "layer two", SIG, Some(base));
        set_ref(&repo, "refs/heads/layer2", l2);

        let err = replay(
            &repo,
            "refs/heads/base",
            &["refs/heads/layer1".into(), "refs/heads/layer2".into()],
            "refs/heads/out",
            sig(),
        )
        .expect_err("overlap must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("layer2"), "got: {msg}");
        assert!(msg.contains("a.txt"), "got: {msg}");
        assert_eq!(ref_id(&repo, "refs/heads/out"), None);
    }
}
