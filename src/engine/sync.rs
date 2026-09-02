//! Branch syncing — the core of the current milestone.
//!
//! The two branches the engine maintains on the fork:
//! - `upstream/<X>` — a pure mirror of upstream's branch `X`. **Fast-forward
//!   only.** Never rewritten. This is the stack trunk that patch PRs target.
//! - `<X>` (fork default) — the recomposed *artifact* (upstream tree + patch
//!   stack + fork-owned files). *Composition is a later milestone; this module
//!   currently focuses on the mirror (sync) half.*
//!
//! All operations are pure functions of repository state so they can be tested
//! against local bare repositories with no network transport.

use gix::Repository;

/// Open the local git working copy of the fork at `path`.
///
/// This is the repository that the app clones/mirrors from GitHub and drives
/// all git operations against.
pub fn open_repo(path: impl Into<std::path::PathBuf>) -> anyhow::Result<Repository> {
    let repo = gix::open(path.into())?;
    Ok(repo)
}

/// The outcome of attempting to fast-forward a ref to a target commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfOutcome {
    /// The ref was already at the target; nothing to do.
    UpToDate,
    /// The ref was fast-forwarded from `from` to `to`.
    FastForwarded { from: String, to: String },
    /// The target is not a descendant of the ref; a fast-forward is impossible
    /// (the mirror would need a rewrite, which we deliberately refuse).
    NotFastForward { current: String, target: String },
}

/// Fast-forward the local reference `ref_name` (e.g. `refs/heads/upstream/main`)
/// to `target` (a commit already present in the object database).
///
/// This is the *mutating* half of mirror syncing: after fetching upstream into
/// the object store, we move the mirror ref only if the move is a strict
/// fast-forward. Rewrites are refused — the mirror must never be rewritten.
pub fn fast_forward(
    repo: &Repository,
    ref_name: &str,
    target: gix::ObjectId,
) -> anyhow::Result<FfOutcome> {
    // Resolve the current value of the ref, if any.
    let current = match repo.find_reference(ref_name) {
        Ok(r) => Some(r.id().detach()),
        Err(_) => None, // ref does not exist yet
    };

    // The merge base between the current ref target and the target tells us
    // whether the move is a fast-forward: FF is possible iff the current commit
    // is an ancestor of the target.
    let outcome = match current {
        None => {
            // Ref doesn't exist -> just create it pointing at the target.
            write_ref(repo, ref_name, target, None)?;
            FfOutcome::FastForwarded {
                from: "(missing)".to_string(),
                to: target.to_string(),
            }
        }
        Some(cur) if cur == target => FfOutcome::UpToDate,
        Some(cur) => {
            let is_ff = is_ancestor(repo, cur, target)?;
            if is_ff {
                write_ref(repo, ref_name, target, Some(cur))?;
                FfOutcome::FastForwarded {
                    from: cur.to_string(),
                    to: target.to_string(),
                }
            } else {
                FfOutcome::NotFastForward {
                    current: cur.to_string(),
                    target: target.to_string(),
                }
            }
        }
    };

    Ok(outcome)
}

/// Write `target` onto the reference named `ref_name`, recording a reflog.
fn write_ref(
    repo: &Repository,
    ref_name: &str,
    target: gix::ObjectId,
    previous: Option<gix::ObjectId>,
) -> anyhow::Result<()> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::Target;

    let constraint = match previous {
        Some(prev) => PreviousValue::ExistingMustMatch(Target::Object(prev)),
        None => PreviousValue::MustNotExist,
    };

    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("fork-maintainer: fast-forward to {target}").into(),
            },
            expected: constraint,
            new: Target::Object(target),
        },
        name: gix::refs::FullName::try_from(ref_name)?,
        deref: false,
    })?;
    Ok(())
}

/// Return true if `ancestor` is an ancestor of (or equal to) `descendant`.
fn is_ancestor(repo: &Repository, ancestor: gix::ObjectId, descendant: gix::ObjectId) -> anyhow::Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    // The best merge base of the two is the deepest common ancestor. The
    // descendant is a fast-forward target iff the current ref IS that common
    // ancestor (i.e. current is an ancestor of target). A missing merge base
    // means the histories are entirely disjoint -> not an ancestor.
    match repo.merge_base(ancestor, descendant) {
        Ok(base) => Ok(base.detach() == ancestor),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::actor::SignatureRef;
    use gix::objs::tree::EntryKind;
    use std::path::Path;

    /// Create an empty, bare git repository at `path` and return the opened repo.
    fn init_bare(path: &Path) -> Repository {
        gix::init_bare(path).expect("init bare")
    }

    /// A static test signature (leaks; fine for tests).
    const SIG: &[u8] = b"tester <tester@example.com> 1711398853 +0000";

    fn sig() -> SignatureRef<'static> {
        SignatureRef::from_bytes(SIG).expect("valid sig")
    }

    /// Write a root commit whose tree contains a single file `name` -> `content`.
    /// If `parent` is `Some`, it becomes the parent of this commit (linear history).
    fn commit_with_file(
        repo: &Repository,
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
        let commit = repo
            .new_commit_as(sig(), sig(), message, tree_id, parent)
            .expect("new commit");
        commit.id.into()
    }

    /// Return the ObjectId the given ref currently points to, or None.
    fn ref_id(repo: &Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    /// Unique temp dir per test so tests can run in parallel.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join("fork-maintainer-test");
        let dir = base.join(format!("{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn creates_ref_when_missing() {
        let dir = temp_dir("creates_ref_when_missing");
        let repo = init_bare(&dir);
        let c1 = commit_with_file(&repo, "a.txt", "one", "commit one", None);

        let outcome = fast_forward(&repo, "refs/heads/upstream/main", c1).expect("ff");
        assert_eq!(
            outcome,
            FfOutcome::FastForwarded {
                from: "(missing)".to_string(),
                to: c1.to_string()
            }
        );
        assert_eq!(ref_id(&repo, "refs/heads/upstream/main"), Some(c1));
    }

    #[test]
    fn fast_forwards_when_descendant() {
        let dir = temp_dir("fast_forwards_when_descendant");
        let repo = init_bare(&dir);
        let c1 = commit_with_file(&repo, "a.txt", "one", "commit one", None);
        fast_forward(&repo, "refs/heads/upstream/main", c1).expect("ff");

        // c2 is a child of c1 (same tree content), so FF is valid.
        let c2 = commit_with_file(&repo, "a.txt", "two", "commit two", Some(c1));
        let outcome = fast_forward(&repo, "refs/heads/upstream/main", c2).expect("ff");
        assert!(matches!(outcome, FfOutcome::FastForwarded { .. }));
        assert_eq!(ref_id(&repo, "refs/heads/upstream/main"), Some(c2));
    }

    #[test]
    fn reports_up_to_date() {
        let dir = temp_dir("reports_up_to_date");
        let repo = init_bare(&dir);
        let c1 = commit_with_file(&repo, "a.txt", "one", "commit one", None);
        fast_forward(&repo, "refs/heads/upstream/main", c1).expect("ff");
        let outcome = fast_forward(&repo, "refs/heads/upstream/main", c1).expect("ff");
        assert_eq!(outcome, FfOutcome::UpToDate);
    }

    #[test]
    fn refuses_diverged_history() {
        let dir = temp_dir("refuses_diverged_history");
        let repo = init_bare(&dir);
        let c1 = commit_with_file(&repo, "a.txt", "one", "commit one", None);
        fast_forward(&repo, "refs/heads/upstream/main", c1).expect("ff");

        // Build an *independent* commit chain that shares no ancestry with c1.
        let independent = commit_with_file(&repo, "b.txt", "other", "independent root", None);
        let outcome = fast_forward(&repo, "refs/heads/upstream/main", independent).expect("ff");

        assert!(matches!(outcome, FfOutcome::NotFastForward { .. }));
        assert_eq!(ref_id(&repo, "refs/heads/upstream/main"), Some(c1));
    }
}
