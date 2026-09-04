//! Fetching from upstream — milestone 2.
//!
//! Pulls the upstream repository into the fork's object store over the git
//! transport (HTTPS in production, `file://` in tests), then reports the new
//! tip of the tracked upstream branch so the caller can fast-forward the
//! mirror ref (`engine::sync::fast_forward`).
//!
//! Fetching and the mirror ref update are deliberately decoupled: fetch only
//! brings objects in and reports where upstream moved; the caller owns the
//! fast-forward policy. This keeps the transport seam testable against local
//! bare repositories with no network.
//!
//! # Blocking
//!
//! These functions use gix's *blocking* network client (`blocking-network-client`
//! feature). They are synchronous and will block the calling thread while the
//! transport does I/O.

use anyhow::Result;
use gix::Repository;
use std::path::Path;

/// The tip of a tracked upstream branch after a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedTip {
    /// The object id of the upstream branch tip after the fetch.
    pub oid: gix::ObjectId,
}

/// Fetch `branch` from the upstream repository at `upstream_url` into `repo`,
/// writing the result into the tracking ref `track_ref` (e.g.
/// `refs/remotes/upstream/main`), and return the new upstream tip.
///
/// `upstream_url` may be an HTTPS URL (production) or a `file://` / local path
/// (tests). The fetch is configured with a single explicit refspec so the
/// upstream branch lands in a ref we control, not git's default
/// `refs/remotes/<name>/...` location.
///
/// Fetching only brings objects in and writes the tracking ref; it does *not*
/// move the `upstream/<X>` mirror ref. The caller owns the fast-forward policy
/// (see [`crate::engine::sync::sync_mirror`]).
///
/// See the [module docs](self) for blocking semantics.
pub fn fetch_upstream(
    repo: &Repository,
    upstream_url: &str,
    branch: &str,
    track_ref: &str, // e.g. "refs/remotes/upstream/main"
) -> Result<FetchedTip> {
    let full_remote_ref = format!("refs/heads/{branch}");
    let oid = fetch_ref(repo, upstream_url, &full_remote_ref, track_ref)?.ok_or_else(|| {
        anyhow::anyhow!(
            "upstream did not advertise branch `{branch}` (fetched via `{upstream_url}`)"
        )
    })?;
    Ok(FetchedTip { oid })
}

/// Fetch a single remote ref from `url` into the local `local_ref`, and return
/// the fetched object id (or `None` if the remote did not advertise it).
///
/// `remote_ref` is the fully-qualified ref name on the remote (e.g.
/// `refs/heads/main`) and `local_ref` is where it lands locally (e.g.
/// `refs/synthesis/base`).
///
/// The fetch uses a single explicit refspec so the ref lands exactly where we
/// control it. Only objects are brought in; no ref policy is applied by this
/// function.
///
/// See the [module docs](self) for blocking semantics.
fn fetch_ref(
    repo: &Repository,
    url: &str,
    remote_ref: &str,
    local_ref: &str,
) -> Result<Option<gix::ObjectId>> {
    use gix::remote::Direction;

    let spec = format!("{remote_ref}:{local_ref}");
    let remote = repo
        .remote_at(url)?
        .with_refspecs([spec.as_str()], Direction::Fetch)?;

    let connection = remote.connect(Direction::Fetch)?;
    let prepare = connection.prepare_fetch(
        gix::progress::Discard,
        gix::remote::ref_map::Options::default(),
    )?;

    let outcome = prepare.receive(gix::progress::Discard, &Default::default())?;

    let oid = outcome
        .ref_map
        .mappings
        .iter()
        .find(|m| remote_ref.eq(m.remote.as_name().unwrap_or_default()))
        .and_then(|m| m.remote.peeled_id())
        .map(gix::oid::to_owned);

    Ok(oid)
}

/// Convenience wrapper: fetch `branch` into `track_ref` from a local bare
/// repo path (used by tests and local development against a filesystem remote).
pub fn fetch_upstream_from_path(
    repo: &Repository,
    upstream_path: &Path,
    branch: &str,
    track_ref: &str,
) -> Result<FetchedTip> {
    let url = upstream_path
        .canonicalize()
        .map(|p| format!("file://{}", p.display()))
        .unwrap_or_else(|_| upstream_path.display().to_string());
    fetch_upstream(repo, &url, branch, track_ref)
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

    /// Write a commit with a single file and an optional parent.
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
        commit.id
    }

    fn ref_id(repo: &Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn fetches_upstream_tip_into_mirror_ref() {
        // Upstream: a bare repo with a `main` branch pointing at a commit.
        let upstream_dir = temp_dir("upstream");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let c1 = commit_with_file(&upstream, "a.txt", "one", "one", None);
        upstream
            .reference(
                "refs/heads/main",
                c1,
                PreviousValue::Any,
                "init upstream branch",
            )
            .expect("create upstream main");

        // Fork: an empty bare repo.
        let fork_dir = temp_dir("fork");
        let fork = gix::init_bare(&fork_dir).expect("init fork");
        assert_eq!(ref_id(&fork, "refs/remotes/upstream/main"), None);

        let tip =
            fetch_upstream_from_path(&fork, &upstream_dir, "main", "refs/remotes/upstream/main")
                .expect("fetch upstream");
        assert_eq!(tip.oid, c1);

        // The fetch wrote the tracking ref for the caller to fast-forward from.
        assert_eq!(ref_id(&fork, "refs/remotes/upstream/main"), Some(c1));
    }

    #[test]
    fn errors_when_upstream_has_no_such_branch() {
        let upstream_dir = temp_dir("upstream_none");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let c1 = commit_with_file(&upstream, "a.txt", "one", "one", None);
        upstream
            .reference(
                "refs/heads/main",
                c1,
                PreviousValue::Any,
                "init upstream branch",
            )
            .expect("create upstream main");

        let fork_dir = temp_dir("fork_none");
        let fork = gix::init_bare(&fork_dir).expect("init fork");

        let err =
            fetch_upstream_from_path(&fork, &upstream_dir, "nope", "refs/remotes/upstream/nope")
                .expect_err("should fail for missing branch");
        // gix rejects a fetch whose explicit refspec matched nothing.
        assert!(
            err.to_string().contains("refs/heads/nope"),
            "unexpected error: {err}"
        );
        // And the tracking ref must not have been created.
        assert_eq!(ref_id(&fork, "refs/remotes/upstream/nope"), None);
    }

    /// The HTTPS transport must be functional: fetching from an unroutable
    /// host must fail at DNS/connection time, never with "unsupported
    /// protocol" (which would mean the transport was built without TLS —
    /// a regression that only manifests outside `file://` tests).
    #[test]
    fn https_transport_supports_https_scheme() {
        let dir = temp_dir("https_probe");
        let repo = gix::init_bare(&dir).expect("init bare");
        let err = fetch_upstream(
            &repo,
            "https://nonexistent.invalid/repo.git",
            "main",
            "refs/synthesis/probe",
        )
        .expect_err("unroutable host must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            !msg.contains("unsupported protocol") && !msg.contains("disabled"),
            "HTTPS transport unavailable: {err:#}"
        );
    }
}
