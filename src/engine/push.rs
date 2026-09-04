//! Pushing the recomposed artifact and mirror ref back to the fork.
//!
//! After the engine recomposes the artifact (`<X>`) and syncs the mirror
//! (`upstream/<X>`), those changes live only in the local mirror. This module
//! pushes the two maintained refs back to the fork on GitHub so users and
//! CI see the recomposed artifact and PRs can target the mirror ref.
//!
//! # Design
//!
//! gix 0.87 does not expose a push implementation in its public API — only
//! fetch. Rather than blocking the milestone on a major gix upgrade, this
//! module shells out to the `git` CLI for the push step.
//!
//! The call is synchronous and blocking, consistent with the rest of the
//! engine (fetch, compose).
//!
//! In tests, pushing against a local bare repository over a filesystem path
//! exercises the full round-trip with no network needed.
//!
//! # Refspecs
//!
//! Two refs are pushed to the fork:
//! - `<X>` (the artifact) → fork's default branch on GitHub
//! - `upstream/<X>` (the mirror) → fork on GitHub (so PRs can target it)

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Outcome of a push to the fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    /// The refspecs that were successfully pushed.
    pub pushed: Vec<String>,
}

/// Push `refspecs` to the fork at `remote_url` from the local repo at
/// `repo_path`.
///
/// Each refspec is of the form `local_ref:remote_ref` (e.g.
/// `refs/heads/main:refs/heads/main`). The push uses the `git` CLI because
/// gix 0.87 does not expose push operations in its public API.
///
/// `auth_header` is an optional `Authorization` header value (e.g.
/// `Bearer <token>`) set via `git -c http.extraHeader`. When `None`, no
/// authentication is configured (suitable for local bare repos in tests).
///
/// When `force` is true each refspec is pushed with a `+` prefix, allowing
/// non-fast-forward updates. Synthesis rewrites the output branch every run,
/// so the output push is always forced.
///
/// This is blocking I/O — call from a worker thread in async contexts.
///
/// # Errors
///
/// Returns an error if the `git push` command fails or cannot be executed.
pub fn push_refs(
    repo_path: &Path,
    remote_url: &str,
    refspecs: &[String],
    auth_header: Option<&str>,
    force: bool,
) -> Result<PushResult> {
    if refspecs.is_empty() {
        return Ok(PushResult { pushed: vec![] });
    }

    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo_path)
        .arg("push")
        .arg("--porcelain")
        .arg(remote_url);

    if let Some(header) = auth_header {
        cmd.arg("-c").arg(format!("http.extraHeader={header}"));
    }

    for spec in refspecs {
        if force && !spec.starts_with('+') {
            cmd.arg(format!("+{spec}"));
        } else {
            cmd.arg(spec);
        }
    }

    let output = cmd.output().context("failed to execute git push")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git push failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pushed = parse_push_output(&stdout);

    Ok(PushResult { pushed })
}

/// Parse the porcelain output of `git push --porcelain`.
///
/// Lines starting with `*` (new/updated), `+` (forced update), or `=`
/// (up-to-date) indicate successfully processed refs. The format is:
/// `<prefix> <from>:<to> [<status>]` where `<to>` is the remote ref.
fn parse_push_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| line.starts_with('*') || line.starts_with('+') || line.starts_with('='))
        .filter_map(|line| {
            // Format: `<prefix>	<from>:<to>	[status]`
            // Split by tab, second field is "from:to"
            let parts: Vec<&str> = line.split('\t').collect();
            parts.get(1).and_then(|spec| {
                // Split "from:to" to get the remote ref
                spec.split(':').nth(1).map(String::from)
            })
        })
        .collect()
}

/// Push the synthesized output ref to its repository.
///
/// `output_ref` is the local ref holding the synthesized commit (e.g.
/// `refs/heads/synthesized`), pushed to `refs/heads/<branch>` at
/// `remote_url`. The push is always forced: synthesis rebuilds the branch
/// from the base every run, so its history is routinely rewritten.
///
/// `auth_header` is an optional `Authorization` header value for
/// authenticated pushes (e.g. `Bearer <token>`).
///
/// # Errors
///
/// Returns an error if the push fails.
pub fn push_output(
    repo_path: &Path,
    remote_url: &str,
    output_ref: &str,
    branch: &str,
    auth_header: Option<&str>,
) -> Result<PushResult> {
    let spec = format!("{output_ref}:refs/heads/{branch}");
    push_refs(repo_path, remote_url, &[spec], auth_header, true)
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
            .join(format!("push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Parse the porcelain output of `git push --porcelain` to extract pushed refs.
    #[test]
    fn parse_push_output_extracts_ok_refs() {
        // Note: git push --porcelain uses tabs, not spaces.
        // Format: `*\t<from>:<to>\t[<status>]`
        let stdout = "To /tmp/target.git\n*\tHEAD:refs/heads/main\t[new branch]\n*\tHEAD:refs/heads/upstream/main\t[new branch]\nDone\n";
        let refs = parse_push_output(stdout);
        assert_eq!(refs, vec!["refs/heads/main", "refs/heads/upstream/main"]);
    }

    /// Handle the `=` prefix for up-to-date refs.
    #[test]
    fn parse_push_output_handles_up_to_date() {
        let stdout = "To /tmp/target.git\n=\tHEAD:refs/heads/main\t[up to date]\nDone\n";
        let refs = parse_push_output(stdout);
        assert_eq!(refs, vec!["refs/heads/main"]);
    }

    /// Handle the `+` prefix for forced updates.
    #[test]
    fn parse_push_output_handles_forced_update() {
        let stdout =
            "To /tmp/target.git\n+\tHEAD:refs/heads/main\tab12cd..34ef56 (forced update)\nDone\n";
        let refs = parse_push_output(stdout);
        assert_eq!(refs, vec!["refs/heads/main"]);
    }

    /// Empty refspec list is a no-op.
    #[test]
    fn push_refs_empty_is_noop() {
        let dir = temp_dir("empty");
        let _repo = gix::init_bare(&dir).expect("init bare");
        let result = push_refs(&dir, "does-not-exist", &[], None, false).expect("noop");
        assert!(result.pushed.is_empty());
    }

    /// Push the synthesized output ref, forcing over a divergent remote.
    #[test]
    fn push_output_forces_over_divergent_remote() {
        // "Output repo" on GitHub — a local bare repo with its own history.
        let remote_dir = temp_dir("output_remote");
        let remote = gix::init_bare(&remote_dir).expect("init remote bare");
        let old = commit_with_file(&remote, "a.txt", "old", "old history", None);
        remote
            .reference("refs/heads/main", old, PreviousValue::Any, "init remote")
            .expect("set remote ref");

        // Local scratch repo with an unrelated synthesized commit.
        let local_dir = temp_dir("output_local");
        let local = gix::init_bare(&local_dir).expect("init local bare");
        let new = commit_with_file(&local, "a.txt", "new", "synthesized", None);
        local
            .reference(
                "refs/heads/synthesized",
                new,
                PreviousValue::Any,
                "init synthesized",
            )
            .expect("set synthesized ref");

        // Non-forced push must fail (unrelated histories).
        let err = push_refs(
            &local_dir,
            &remote_dir.display().to_string(),
            &["refs/heads/synthesized:refs/heads/main".to_string()],
            None,
            false,
        )
        .expect_err("non-force push over divergent history should fail");
        assert!(
            err.to_string().contains("rejected"),
            "unexpected error: {err}"
        );

        // push_output forces through.
        let result = push_output(
            &local_dir,
            &remote_dir.display().to_string(),
            "refs/heads/synthesized",
            "main",
            None,
        )
        .expect("push_output");

        assert_eq!(result.pushed, vec!["refs/heads/main".to_string()]);

        let remote = gix::open(&remote_dir).expect("open remote");
        assert_eq!(ref_id(&remote, "refs/heads/main"), Some(new));
    }

    /// Push fails when the remote URL is invalid.
    #[test]
    fn push_refs_fails_on_bad_remote() {
        let dir = temp_dir("bad_remote");
        let mirror = gix::init_bare(&dir).expect("init bare");
        let c1 = commit_with_file(&mirror, "a.txt", "a1", "c", None);
        mirror
            .reference("refs/heads/main", c1, PreviousValue::Any, "init")
            .expect("set ref");

        let result = push_refs(
            &dir,
            "file:///nonexistent/path.git",
            &["refs/heads/main:refs/heads/main".to_string()],
            None,
            false,
        );
        assert!(result.is_err(), "should fail for invalid remote");
    }

    /// Push only the artifact ref (single refspec).
    #[test]
    fn push_refs_single_refspec() {
        let fork_dir = temp_dir("fork_single");
        let _fork = gix::init_bare(&fork_dir).expect("init fork bare");

        let mirror_dir = temp_dir("mirror_single");
        let mirror = gix::init_bare(&mirror_dir).expect("init mirror bare");

        let c1 = commit_with_file(&mirror, "a.txt", "a1", "c", None);
        mirror
            .reference("refs/heads/main", c1, PreviousValue::Any, "init")
            .expect("set ref");

        let result = push_refs(
            &mirror_dir,
            &fork_dir.display().to_string(),
            &["refs/heads/main:refs/heads/main".to_string()],
            None,
            false,
        )
        .expect("push single ref");

        assert_eq!(result.pushed, vec!["refs/heads/main".to_string()]);

        let fork = gix::open(&fork_dir).expect("open fork");
        assert_eq!(ref_id(&fork, "refs/heads/main"), Some(c1));
    }
}
