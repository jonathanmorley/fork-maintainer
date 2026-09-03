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
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Serializes `ensure_mirror` per mirror path so concurrent reconcile calls
/// (e.g. a webhook and the poll loop firing for the same fork at once) cannot
/// both attempt to `git clone` into the same directory.
static CLONE_LOCKS: OnceLock<Mutex<HashMap<std::path::PathBuf, std::sync::Arc<Mutex<()>>>>> =
    OnceLock::new();

fn clone_lock_for(path: &Path) -> std::sync::Arc<Mutex<()>> {
    let map = CLONE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().expect("clone-lock map poisoned");
    map.entry(path.to_path_buf())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

/// Ensure a bare mirror clone exists at `mirror_path`.
///
/// If `mirror_path` already exists and looks like a bare git repo, this is a
/// no-op. Otherwise, clones `remote_url` as a bare mirror into `mirror_path`.
///
/// `auth_header` is an optional `Authorization` header value for
/// authenticated clones (e.g. `Bearer <token>`).
///
/// On failure the partially-cloned directory is removed so the next attempt
/// starts from a clean slate.
///
/// Concurrent calls for the same `mirror_path` are serialized so only one
/// clone runs at a time.
///
/// This is blocking I/O — call from a worker thread in async contexts.
pub fn ensure_mirror(
    mirror_path: &Path,
    remote_url: &str,
    auth_header: Option<&str>,
) -> Result<()> {
    // Serialize concurrent first-boot clones for the same path. Cheap after
    // the initial healthy check: the check-and-exists happens under the lock.
    let lock = clone_lock_for(mirror_path);
    let _guard = lock.lock().expect("clone lock poisoned");

    // Re-check under the lock: another call may have cloned it while we waited.
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
        // Clean up any partial clone so the next attempt starts fresh.
        let _ = std::fs::remove_dir_all(mirror_path);
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
    // Bare repo: the path itself is the git directory. Require the core files
    // a usable bare repo always has, to avoid mistaking a partial clone or an
    // ad-hoc directory holding just HEAD + objects for a real repo.
    if path.join("HEAD").exists()
        && path.join("objects").exists()
        && path.join("refs").exists()
        && path.join("config").exists()
    {
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
