//! Live reconcile orchestration — ties the GitHub layer to the git engine.
//!
//! This is the "full engine" seam: for a configured fork it wires the pieces a
//! deployment actually runs — GitHub App auth (installation client + HTTPS
//! token), live open-PR discovery, PR-head fetch into the local mirror, and the
//! engine's sync + compose pipeline — into one `reconcile`. Both the webhook
//! dispatcher and the poll loop call it to drive a fork.
//!
//! The core that can run without a network ([`reconcile_discovered`]) is fully
//! unit-tested against local bare repositories (fetch over the `file://`
//! transport). The async, network-dependent wrapper ([`reconcile_fork_live`])
//! is intentionally thin: discover PRs via octocrab, then run the blocking git
//! phase on a worker thread.

use anyhow::{Context, Result};
use gix::actor::SignatureRef;

use crate::config::ForkConfig;
use crate::engine::fetch::fetch_pull_refs;
use crate::engine::pipeline::{ReconcileOutcome, reconcile};
use crate::engine::sync::open_repo;
use crate::github::PrInfo;
use crate::github::auth::AppCredentials;
use crate::github::discovery::discover_stack;

/// Reconcile a fork from its *discovered* PR stack (no network).
///
/// `repo` is the open local mirror. `upstream_url` and `fork_url` are the
/// transport URLs of the upstream repository (e.g. `file://` in tests,
/// `cfg.upstream.https_url()` in production) and the fork (used to fetch PR
/// heads; in production this is the fork URL authenticated with
/// [`crate::config::Repo::authed_https_url`]).
///
/// `prs` are the fork's open pull requests (produced by
/// [`crate::github::live_prs`]); the ordered stack is derived from them (plus
/// the fork-owned branch, if any — the bottom layer) via
/// [`discover_stack`], the PR heads are fetched into the mirror, and then the
/// engine's [`reconcile`] syncs upstream + composes the artifact.
///
/// This is the testable core of the milestone: it exercises the discovery ->
/// fetch -> compose wiring against local repositories with no GitHub needed.
///
/// # Blocking
///
/// Uses blocking gix transport and object I/O; wrap in
/// [`tokio::task::spawn_blocking`] from an async context.
pub fn reconcile_discovered(
    repo: &gix::Repository,
    cfg: &ForkConfig,
    upstream_url: &str,
    fork_url: &str,
    prs: &[PrInfo],
    committer: SignatureRef<'_>,
) -> Result<ReconcileOutcome> {
    let upstream_branch = cfg.upstream_branch();
    let stack = discover_stack(&upstream_branch, cfg.fork_owned_branch.as_deref(), prs)
        .map_err(|e| anyhow::anyhow!("discover stack: {e}"))?;

    // Bring the discovered PR heads into the local mirror so compose can layer
    // them. The fork-owned branch is expected to already exist locally.
    fetch_pull_refs(repo, fork_url, &stack)?;

    reconcile(repo, cfg, upstream_url, &stack, committer)
}

/// Reconcile a fork against live GitHub, end to end.
///
/// Steps, in order:
/// 1. Authenticate: build an installation-scoped octocrab client for the fork
///    and mint a short-lived installation HTTPS token for git access.
/// 2. Discover the fork's open PRs over the API.
/// 3. Run the blocking git phase (open local mirror, fetch PR heads, sync
///    upstream, compose artifact) on a worker thread, using the token-
///    authenticated fork URL.
///
/// Returns the engine's [`ReconcileOutcome`], or an error if any step fails
/// (auth, discovery, or git). Errors are coarse here; the caller (poll loop /
/// webhook) decides how to surface them.
pub async fn reconcile_fork_live(
    app: &AppCredentials,
    cfg: &ForkConfig,
    committer: SignatureRef<'static>,
) -> Result<ReconcileOutcome> {
    let app_client = app.app_client()?;
    let install =
        crate::github::auth::install_client(&app_client, &cfg.fork.owner, &cfg.fork.name).await?;
    let prs = crate::github::discovery::live_prs(&install, &cfg.fork).await?;
    let token =
        crate::github::auth::install_https_token(&app_client, &cfg.fork.owner, &cfg.fork.name)
            .await?;

    let upstream_url = cfg.upstream.https_url();
    let fork_url = cfg.fork.authed_https_url(&token);

    let fork_cfg = cfg.clone();
    let local_mirror = cfg
        .local_mirror
        .clone()
        .context("fork has no local_mirror configured")?;
    let prs = prs.clone();

    tokio::task::spawn_blocking(move || {
        // Ensure the local mirror exists (first-boot clone if needed).
        crate::mirror::ensure_mirror(
            std::path::Path::new(&local_mirror),
            &fork_url,
            None, // Auth is already embedded in fork_url via x-access-token
        )?;

        let repo = open_repo(&local_mirror)?;
        reconcile_discovered(&repo, &fork_cfg, &upstream_url, &fork_url, &prs, committer)
    })
    .await
    .context("git reconcile phase panicked")?
}

/// Reconcile a fork and push the recomposed artifact + mirror back to GitHub.
///
/// This is the full end-to-end pipeline: authenticate, discover PRs, fetch,
/// sync, compose, and push. It combines [`reconcile_fork_live`] with
/// [`crate::engine::push::push_fork_refs`].
///
/// The push happens on the same worker thread as the git phase. If the push
/// fails (e.g. the remote has been modified), the reconcile outcome is still
/// returned — the push error is surfaced separately.
///
/// # Blocking
///
/// The git phase (fetch, sync, compose, push) is blocking I/O; this function
/// must be called from a worker thread in async contexts.
pub async fn reconcile_and_push_live(
    app: &AppCredentials,
    cfg: &ForkConfig,
    committer: SignatureRef<'static>,
) -> Result<(ReconcileOutcome, Option<crate::engine::push::PushResult>)> {
    let app_client = app.app_client()?;
    let install =
        crate::github::auth::install_client(&app_client, &cfg.fork.owner, &cfg.fork.name).await?;
    let prs = crate::github::discovery::live_prs(&install, &cfg.fork).await?;
    let token =
        crate::github::auth::install_https_token(&app_client, &cfg.fork.owner, &cfg.fork.name)
            .await?;

    let upstream_url = cfg.upstream.https_url();
    let fork_url = cfg.fork.authed_https_url(&token);

    let fork_cfg = cfg.clone();
    let local_mirror = cfg
        .local_mirror
        .clone()
        .context("fork has no local_mirror configured")?;
    let prs = prs.clone();

    tokio::task::spawn_blocking(move || {
        // Ensure the local mirror exists (first-boot clone if needed).
        crate::mirror::ensure_mirror(
            std::path::Path::new(&local_mirror),
            &fork_url,
            None, // Auth is already embedded in fork_url via x-access-token
        )?;

        let repo = open_repo(&local_mirror)?;

        // Phase 1: discover -> fetch -> sync -> compose.
        let outcome =
            reconcile_discovered(&repo, &fork_cfg, &upstream_url, &fork_url, &prs, committer)?;

        // Phase 2: push the recomposed artifact and mirror back to the fork.
        let mirror_ref = format!("refs/heads/{}", fork_cfg.mirror_branch());
        let artifact_ref = format!("refs/heads/{}", fork_cfg.default_branch);
        let push_result = crate::engine::push::push_fork_refs(
            repo.workdir().unwrap_or_else(|| repo.common_dir()),
            &fork_url,
            &artifact_ref,
            &mirror_ref,
            &fork_cfg.default_branch,
            None, // Auth is already embedded in fork_url via x-access-token
        );

        match push_result {
            Ok(push) => Ok((outcome, Some(push))),
            Err(e) => {
                tracing::warn!(fork = %fork_cfg.fork, "push failed after reconcile: {e}");
                Ok((outcome, None))
            }
        }
    })
    .await
    .context("git reconcile+push phase panicked")?
}

#[cfg(test)]
mod tests {
    use super::*;
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
            .join(format!("{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn commit_with_files(
        repo: &gix::Repository,
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

    fn ref_id(repo: &gix::Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    fn set_ref(repo: &gix::Repository, name: &str, target: gix::ObjectId) {
        repo.reference(name, target, PreviousValue::Any, "set ref for test")
            .expect("set ref");
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

    fn tree_has_entry(repo: &gix::Repository, tree_id: gix::ObjectId, path: &str) -> bool {
        let mut tree = repo.find_tree(tree_id).expect("find tree");
        tree.peel_to_entry(path.split('/')).expect("peel").is_some()
    }

    fn mkcfg(fork_owned: Option<&str>) -> ForkConfig {
        ForkConfig {
            upstream: crate::config::Repo {
                owner: "integrations".into(),
                name: "terraform-provider-github".into(),
            },
            fork: crate::config::Repo {
                owner: "myorg".into(),
                name: "terraform-provider-github".into(),
            },
            default_branch: "main".into(),
            local_mirror: None,
            override_upstream_branch: None,
            fork_owned_branch: fork_owned.map(str::to_string),
        }
    }

    fn pr(number: u64, head: &str, base: &str) -> PrInfo {
        PrInfo {
            number,
            head_branch: head.into(),
            base_branch: base.into(),
        }
    }

    #[test]
    fn reconcile_discovered_layers_pulls_over_upstream_and_fork_owned() {
        // Upstream: c1 (a.txt) then advances to c2 (a.txt + b.txt).
        let upstream_dir = temp_dir("rec_upstream");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let c1 = commit_with_files(&upstream, &[("a.txt", "a1")], "c1", None);
        set_ref(&upstream, "refs/heads/main", c1);
        let c2 = commit_with_files(
            &upstream,
            &[("a.txt", "a1"), ("b.txt", "b2")],
            "c2",
            Some(c1),
        );
        set_ref(&upstream, "refs/heads/main", c2);

        // Fork remote: the source of PR heads. A root PR commit (self-contained
        // object) avoids cross-repo parent references.
        let fork_remote_dir = temp_dir("rec_fork_remote");
        let fork_remote = gix::init_bare(&fork_remote_dir).expect("init fork remote");
        let pr12 = commit_with_files(
            &fork_remote,
            &[("a.txt", "a1"), ("feat.txt", "f")],
            "feat-a",
            None,
        );
        set_ref(&fork_remote, "refs/pull/12/head", pr12);

        // Local mirror (what the poll/webhook drives): starts with the
        // fork-owned branch already present locally (its commit lives in the
        // mirror's own object store).
        let mirror_dir = temp_dir("rec_mirror");
        let mirror = gix::init_bare(&mirror_dir).expect("init mirror");
        let owned = commit_with_files(
            &mirror,
            &[("a.txt", "a1"), (".github/ci.yml", "workflow")],
            "owned",
            None,
        );
        set_ref(&mirror, "refs/heads/fork-owned", owned);

        let cfg = mkcfg(Some("fork-owned"));
        let prs = vec![pr(12, "feat-a", "main")];

        // The upstream URL is the local upstream path; the fork URL (where PR
        // heads come from) is the local fork "remote" path.
        let out = reconcile_discovered(
            &mirror,
            &cfg,
            &upstream_dir.display().to_string(),
            &fork_remote_dir.display().to_string(),
            &prs,
            sig(),
        )
        .expect("reconcile_discovered");

        // Mirror synced to upstream tip.
        assert_eq!(ref_id(&mirror, "refs/heads/upstream/main"), Some(c2));

        // PR head fetched into the mirror.
        assert_eq!(ref_id(&mirror, "refs/pull/12/head"), Some(pr12));

        // Artifact carries upstream content + fork-owned layer + PR change.
        assert_eq!(
            tree_blob(&mirror, out.compose.tree, "a.txt").as_deref(),
            Some("a1")
        );
        assert_eq!(
            tree_blob(&mirror, out.compose.tree, "b.txt").as_deref(),
            Some("b2")
        );
        assert_eq!(
            tree_blob(&mirror, out.compose.tree, ".github/ci.yml").as_deref(),
            Some("workflow")
        );
        assert_eq!(
            tree_blob(&mirror, out.compose.tree, "feat.txt").as_deref(),
            Some("f")
        );
    }

    #[test]
    fn reconcile_discovered_without_fork_owned_layers_pull_only() {
        let upstream_dir = temp_dir("rec_up2");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let c1 = commit_with_files(&upstream, &[("a.txt", "a1")], "c1", None);
        set_ref(&upstream, "refs/heads/main", c1);

        let fork_remote_dir = temp_dir("rec_fork_remote2");
        let fork_remote = gix::init_bare(&fork_remote_dir).expect("init fork remote");
        let pr9 = commit_with_files(
            &fork_remote,
            &[("a.txt", "a1"), ("x.txt", "x")],
            "pr9",
            None,
        );
        set_ref(&fork_remote, "refs/pull/9/head", pr9);

        let mirror_dir = temp_dir("rec_mirror2");
        let mirror = gix::init_bare(&mirror_dir).expect("init mirror");

        let cfg = mkcfg(None);
        let out = reconcile_discovered(
            &mirror,
            &cfg,
            &upstream_dir.display().to_string(),
            &fork_remote_dir.display().to_string(),
            &[pr(9, "x-branch", "main")],
            sig(),
        )
        .expect("reconcile_discovered");

        assert_eq!(
            tree_blob(&mirror, out.compose.tree, "x.txt").as_deref(),
            Some("x")
        );
        // No fork-owned layer configured, so `.github/ci.yml` is absent.
        assert!(!tree_has_entry(&mirror, out.compose.tree, ".github/ci.yml"));
    }
}
