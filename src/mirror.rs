//! First-boot mirror initialization — clones fork repos into local storage.
//!
//! On Render (and other ephemeral environments), the local filesystem is
//! wiped on each deploy. This module provides [`ensure_mirror`] to clone
//! each fork's repository into its configured `local_mirror` path if the
//! directory doesn't exist yet.
//!
//! The clone is a bare mirror (`--mirror`) so the app can fetch/push
//! refs without a working tree.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Ensure a bare mirror clone exists at `mirror_path`.
///
/// If `mirror_path` already exists and contains a `.git` directory (or is
/// itself a bare repo), this is a no-op. Otherwise, clones `remote_url`
/// as a bare mirror into `mirror_path`.
///
/// `auth_header` is an optional `Authorization` header value for
/// authenticated clones (e.g. `Bearer <token>`).
///
/// This is blocking I/O — call from a worker thread in async contexts.
pub fn ensure_mirror(
    mirror_path: &Path,
    remote_url: &str,
    auth_header: Option<&str>,
) -> Result<()> {
    // Check if the mirror already exists and looks like a git repo.
    if is_git_repo(mirror_path) {
        tracing::debug!(path = %mirror_path.display(), "mirror already exists");
        return Ok(());
    }

    tracing::info!(
        path = %mirror_path.display(),
        remote = remote_url,
        "cloning mirror repository"
    );

    // Create parent directory if needed.
    if let Some(parent) = mirror_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    }

    let mut cmd = Command::new("git");
    cmd.arg("clone")
        .arg("--mirror")
        .arg(remote_url)
        .arg(mirror_path);

    if let Some(header) = auth_header {
        cmd.arg("-c").arg(format!("http.extraHeader={header}"));
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to execute git clone for {}", mirror_path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git clone failed for {}: {stderr}", mirror_path.display());
    }

    tracing::info!(
        path = %mirror_path.display(),
        "mirror repository cloned successfully"
    );

    Ok(())
}

/// Check if a path is a git repository (bare or with .git directory).
fn is_git_repo(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    // Bare repo: the path itself is the git directory.
    if path.join("HEAD").exists() && path.join("objects").exists() {
        return true;
    }
    // Non-bare repo: .git directory exists.
    path.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("mirror-init-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn ensure_mirror_clones_bare_repo() {
        let upstream_dir = temp_dir("upstream");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");

        // Create a commit so the repo isn't empty.
        let blob = upstream.write_blob("hello").expect("write blob");
        let mut editor = upstream
            .edit_tree(upstream.empty_tree().id)
            .expect("edit tree");
        editor
            .upsert(
                "greeting.txt",
                gix::objs::tree::EntryKind::Blob,
                blob.detach(),
            )
            .expect("upsert");
        let tree_id = editor.write().expect("write tree").detach();
        let sig =
            gix::actor::SignatureRef::from_bytes(b"tester <tester@example.com> 1711398853 +0000")
                .expect("sig");
        upstream
            .new_commit_as(sig, sig, "init", tree_id, None::<gix::ObjectId>)
            .expect("commit");

        let mirror_dir = temp_dir("mirror_target");

        ensure_mirror(&mirror_dir, &upstream_dir.display().to_string(), None)
            .expect("ensure_mirror");

        assert!(is_git_repo(&mirror_dir));
        assert!(mirror_dir.join("HEAD").exists());
    }

    #[test]
    fn ensure_mirror_skips_existing_repo() {
        let dir = temp_dir("existing");
        gix::init_bare(&dir).expect("init bare");

        // Should not error or re-clone.
        ensure_mirror(&dir, "does-not-matter", None).expect("no-op");
    }

    #[test]
    fn is_git_repo_returns_false_for_nonexistent() {
        let dir = temp_dir("nonexistent");
        assert!(!is_git_repo(&dir));
    }

    #[test]
    fn is_git_repo_returns_true_for_bare() {
        let dir = temp_dir("bare_check");
        gix::init_bare(&dir).expect("init bare");
        assert!(is_git_repo(&dir));
    }
}
