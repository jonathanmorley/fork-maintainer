//! Patch lockfile — pin untrusted patch refs to exact SHAs.
//!
//! Patch branches may live in repositories the synthesizer operator does not
//! control. Auto-following a branch tip means anyone with write access to a
//! patch repo silently lands code on the output branch at the next run, with
//! no review gate and no record of what was included. The lockfile closes
//! that hole: each patch resolves to a recorded SHA, and any drift fails the
//! run before anything composes or pushes.
//!
//! Deliberate asymmetry: the **base floats** (tracking a branch tip is the
//! declared intent — "declare a base branch"), while **patches are pinned**
//! (an ordered set of *known* changes). The lockfile therefore records
//! patches only; the base SHA is logged every run for audit, not enforced.
//!
//! Semantics (Cargo `--locked` style, but default-on):
//! - Lockfile present: every patch tip must equal its recorded SHA, else the
//!   run fails naming the drifted patch (expected vs actual). New patches
//!   absent from the lock also fail.
//! - Lockfile missing: bootstrap — resolve, write, and warn loudly. The
//!   first lock still needs human review of what got pinned.
//! - `--update-lock`: re-resolve all patches, rewrite the lockfile, proceed.
//!   In CI this pairs with committing the lockfile back (reviewable diff).
//!
//! The lockfile must be **committed** (e.g. next to the caller workflow or
//! config) — ephemeral runners start every run without one, which would
//! reduce enforcement to a perpetual bootstrap warning.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::config::BranchRef;

/// One pinned patch: the branch plus the exact commit it resolved to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LockedPatch {
    /// The repository holding the branch.
    pub repo: crate::config::Repo,
    /// The branch name.
    pub branch: String,
    /// The pinned commit SHA (hex).
    pub sha: String,
}

/// The lockfile: pinned SHAs for every patch, in no significant order.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Lockfile {
    /// Pinned patches.
    #[serde(default)]
    pub patches: Vec<LockedPatch>,
}

impl Lockfile {
    /// Build a lockfile from resolved (branch, tip) pairs.
    pub fn from_resolved(resolved: &[(BranchRef, gix::ObjectId)]) -> Self {
        Self {
            patches: resolved
                .iter()
                .map(|(branch, oid)| LockedPatch {
                    repo: branch.repo.clone(),
                    branch: branch.branch.clone(),
                    sha: oid.to_string(),
                })
                .collect(),
        }
    }

    /// Find the recorded SHA for a patch, matched by repo slug and branch.
    fn lookup(&self, patch: &BranchRef) -> Option<&str> {
        self.patches
            .iter()
            .find(|p| p.repo.slug() == patch.repo.slug() && p.branch == patch.branch)
            .map(|p| p.sha.as_str())
    }

    /// Enforce the lock: every resolved tip must equal its recorded SHA.
    ///
    /// Fails on drift (expected vs actual named), on patches absent from the
    /// lock, and never touches extra lock entries (stale entries are pruned
    /// on the next `--update-lock` rewrite).
    ///
    /// # Errors
    ///
    /// Returns an error describing each drifted or unpinned patch.
    pub fn check(&self, resolved: &[(BranchRef, gix::ObjectId)]) -> Result<()> {
        let mut problems = Vec::new();
        for (patch, oid) in resolved {
            // Re-parse instead of re-formatting: no allocation per patch,
            // and a malformed recorded SHA fails closed as drift.
            let matches = match self.lookup(patch) {
                Some(sha) => gix::ObjectId::from_hex(sha.as_bytes()).ok().as_ref() == Some(oid),
                None => false,
            };
            if matches {
                continue;
            }
            match self.lookup(patch) {
                Some(sha) => problems.push(format!(
                    "{patch} moved: locked at {sha}, branch tip is now {oid}"
                )),
                None => problems.push(format!(
                    "{patch} is not in the lockfile; re-lock with --update-lock"
                )),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "patch lockfile drift ({}):\n  {}",
                problems.len(),
                problems.join("\n  ")
            )
        }
    }
}

/// Load a lockfile, or `Ok(None)` when the path does not exist (bootstrap).
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load(path: &Path) -> Result<Option<Lockfile>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            Ok(Some(serde_json::from_str(&raw).with_context(|| {
                format!("parse lockfile {}", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read lockfile {}", path.display())),
    }
}

/// Write a lockfile, creating parent directories as needed.
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn save(path: &Path, lock: &Lockfile) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create lockfile dir {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(lock).context("serialize lockfile")?;
    std::fs::write(path, format!("{raw}\n"))
        .with_context(|| format!("write lockfile {}", path.display()))?;
    Ok(())
}

/// How patch pins are enforced for a run.
#[derive(Debug, Clone, Default)]
pub struct LockOptions {
    /// Lockfile path. `None` disables enforcement (auto-follow, the legacy
    /// behavior); the CLI always sets a path.
    pub path: Option<PathBuf>,
    /// Re-resolve every patch and rewrite the lockfile instead of enforcing.
    pub update: bool,
}

/// Enforce (or bootstrap/update) patch pins for resolved tips.
///
/// - `update` set: write the lockfile from `resolved` and proceed.
/// - Lockfile present: [`Lockfile::check`] the tips against it.
/// - Lockfile missing: bootstrap — write it from `resolved` with a loud
///   warning naming every pinned SHA (review what got pinned).
/// - No path configured: no-op (auto-follow).
///
/// # Errors
///
/// Returns an error on drift, on I/O failures, and when the existing
/// lockfile cannot be parsed.
pub fn enforce(opts: &LockOptions, resolved: &[(BranchRef, gix::ObjectId)]) -> Result<()> {
    let Some(path) = &opts.path else {
        return Ok(());
    };
    if opts.update {
        let lock = Lockfile::from_resolved(resolved);
        save(path, &lock)?;
        tracing::info!(path = %path.display(), "lockfile updated");
        for patch in &lock.patches {
            tracing::info!(patch = %format!("{}/{}", patch.repo.slug(), patch.branch), sha = %patch.sha, "pinned");
        }
        return Ok(());
    }
    match load(path)? {
        Some(lock) => {
            lock.check(resolved)?;
            tracing::info!(path = %path.display(), patches = resolved.len(), "patch lockfile enforced");
            Ok(())
        }
        None => {
            let lock = Lockfile::from_resolved(resolved);
            save(path, &lock)?;
            tracing::warn!(
                path = %path.display(),
                "no lockfile found; bootstrapped one — review the pinned SHAs"
            );
            for patch in &lock.patches {
                tracing::warn!(patch = %format!("{}/{}", patch.repo.slug(), patch.branch), sha = %patch.sha, "pinned");
            }
            Ok(())
        }
    }
}

/// Resolve the default lockfile path: `synthesis.lock` next to `--config`,
/// else `./synthesis.lock`.
pub fn default_path(config_path: Option<&Path>) -> PathBuf {
    match config_path {
        Some(p) => match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join("synthesis.lock"),
            _ => PathBuf::from("synthesis.lock"),
        },
        None => PathBuf::from("synthesis.lock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Repo;

    fn repo(owner: &str, name: &str) -> Repo {
        Repo {
            owner: owner.into(),
            name: name.into(),
        }
    }

    fn branch(owner: &str, name: &str, branch: &str) -> BranchRef {
        BranchRef {
            repo: repo(owner, name),
            branch: branch.into(),
        }
    }

    fn oid(byte: u8) -> gix::ObjectId {
        gix::ObjectId::from_hex(format!("{byte:02x}").repeat(20).as_bytes()).expect("oid")
    }

    fn locked(owner: &str, name: &str, branch: &str, byte: u8) -> LockedPatch {
        LockedPatch {
            repo: repo(owner, name),
            branch: branch.into(),
            sha: oid(byte).to_string(),
        }
    }

    #[test]
    fn check_passes_on_exact_match() {
        let lock = Lockfile {
            patches: vec![locked("me", "r", "feat", 0xab)],
        };
        let resolved = vec![(branch("me", "r", "feat"), oid(0xab))];
        lock.check(&resolved).expect("exact match passes");
    }

    #[test]
    fn check_fails_on_drift_naming_patch_and_shas() {
        let lock = Lockfile {
            patches: vec![locked("me", "r", "feat", 0xab)],
        };
        let resolved = vec![(branch("me", "r", "feat"), oid(0xcd))];
        let err = lock.check(&resolved).expect_err("drift must fail");
        let msg = err.to_string();
        assert!(msg.contains("me/r@feat"), "names patch, got: {msg}");
        assert!(
            msg.contains(&oid(0xab).to_string()),
            "names locked sha, got: {msg}"
        );
        assert!(
            msg.contains(&oid(0xcd).to_string()),
            "names actual sha, got: {msg}"
        );
    }

    #[test]
    fn check_fails_for_unpinned_patch() {
        let lock = Lockfile { patches: vec![] };
        let resolved = vec![(branch("me", "r", "new"), oid(0xab))];
        let err = lock.check(&resolved).expect_err("unpinned must fail");
        assert!(err.to_string().contains("--update-lock"), "got: {err}");
    }

    #[test]
    fn check_ignores_stale_lock_entries() {
        let lock = Lockfile {
            patches: vec![
                locked("me", "r", "feat", 0xab),
                locked("me", "r", "removed", 0xcd),
            ],
        };
        let resolved = vec![(branch("me", "r", "feat"), oid(0xab))];
        lock.check(&resolved).expect("stale entries ignored");
    }

    #[test]
    fn from_resolved_round_trips_through_json() {
        let resolved = vec![
            (branch("me", "r", "a"), oid(0x01)),
            (branch("you", "r", "b"), oid(0x02)),
        ];
        let lock = Lockfile::from_resolved(&resolved);
        let raw = serde_json::to_string(&lock).expect("serialize");
        let back: Lockfile = serde_json::from_str(&raw).expect("parse");
        assert_eq!(lock, back);
        back.check(&resolved).expect("round-tripped lock enforces");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("lockfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("synthesis.lock");
        let lock = Lockfile {
            patches: vec![locked("me", "r", "feat", 0xab)],
        };
        save(&path, &lock).expect("save creates parents");
        let back = load(&path).expect("load").expect("present");
        assert_eq!(lock, back);
    }

    #[test]
    fn load_missing_is_bootstrap_none() {
        let dir = std::env::temp_dir().join(format!("lockfile-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(load(&dir.join("nope.lock")).expect("load").is_none());
    }

    #[test]
    fn default_path_sits_next_to_config() {
        assert_eq!(
            default_path(Some(Path::new("/x/synthesis.json"))),
            PathBuf::from("/x/synthesis.lock")
        );
        assert_eq!(
            default_path(Some(Path::new("synthesis.json"))),
            PathBuf::from("synthesis.lock")
        );
        assert_eq!(default_path(None), PathBuf::from("synthesis.lock"));
    }
}
