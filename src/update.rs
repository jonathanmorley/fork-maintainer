//! Maintenance updates — propose input changes, never mutate live state.
//!
//! The `update` subcommand inspects the declared world against reality and
//! rewrites local files (caller config + lockfile) for human review:
//!
//! - Patch tips that moved since the lockfile → refreshed SHAs.
//! - Patches whose pull requests merged → removed from the declared list.
//!
//! It never pushes output, deletes branches, or closes pull requests. The
//! caller workflow diffs the rewritten files and opens a PR; synthesis
//! itself stays read-only on inputs. A branch that vanished without a merged
//! PR is an error (fail closed — a human investigates).
//!
//! Resolution uses the `git` CLI (`ls-remote` works against `file://`
//! remotes, so this is unit-testable); PR state comes from the GitHub REST
//! API via blocking reqwest.

use anyhow::{Context, Result};
use std::path::Path;

use crate::config::{BranchRef, SynthesisConfig};
use crate::lockfile::Lockfile;

/// A pull request reduced to what update decisions need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    /// Pull request number (for log lines and PR bodies).
    pub number: u64,
    /// True when the PR was merged (as opposed to closed unmerged).
    pub merged: bool,
}

/// Observed live state for one declared patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchStatus {
    /// The declared patch.
    pub patch: BranchRef,
    /// Whether the patch is pinned (lockfile-enforced).
    pub pin: bool,
    /// Current branch tip, or `None` when the branch no longer exists.
    pub tip: Option<gix::ObjectId>,
    /// Associated PRs (by head branch, in the patch's repo and the output
    /// repo), if any.
    pub prs: Vec<PrSummary>,
}

/// File changes the update proposes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdatePlan {
    /// Patches to drop from the declared list (with the merged PR number
    /// that justifies each removal).
    pub removed: Vec<(BranchRef, u64)>,
    /// Whether any surviving patch tip differs from the lockfile.
    pub lock_changed: bool,
}

impl UpdatePlan {
    /// True when there is nothing to write (no removals, lock current).
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && !self.lock_changed
    }
}

/// Decide file updates from observed patch states plus the lockfile.
///
/// `statuses` covers every declared patch; `lock` is the current lockfile
/// (or `None` on first run, when everything is new).
///
/// Rules, in order per patch:
/// 1. Merged associated PR → remove from the declared list (whether or not
///    the branch still exists — the merge is the justification).
/// 2. Branch gone without a merged PR → error, fail closed.
/// 3. Surviving tip differs from the locked SHA (or no lock entry) → the
///    lock needs rewriting.
///
/// # Errors
///
/// Returns an error naming patches whose branch vanished without a merged PR.
pub fn plan_updates(statuses: &[PatchStatus], lock: Option<&Lockfile>) -> Result<UpdatePlan> {
    let mut plan = UpdatePlan::default();
    for status in statuses {
        if let Some(pr) = status.prs.iter().find(|pr| pr.merged) {
            plan.removed.push((status.patch.clone(), pr.number));
            continue;
        }
        let Some(tip) = status.tip else {
            anyhow::bail!(
                "patch {} no longer exists and has no merged PR; refusing to guess — investigate manually",
                status.patch
            );
        };
        // Unpinned patches float by design: nothing to record, nothing to
        // compare. Their movement is expected, not drift.
        if !status.pin {
            continue;
        };
        let locked = lock.and_then(|l| {
            l.patches.iter().find_map(|p| {
                (p.repo.slug() == status.patch.repo.slug() && p.branch == status.patch.branch)
                    .then(|| p.sha.clone())
            })
        });
        if locked.as_deref() != Some(tip.to_string()).as_deref() {
            plan.lock_changed = true;
        }
    }
    Ok(plan)
}

/// Resolve a branch tip over the transport without fetching objects.
///
/// Shells out to `git ls-remote` (push module precedent); works against
/// `file://` remotes, so tests run offline.
///
/// Returns `Ok(None)` when the branch is not advertised.
pub fn resolve_tip(url: &str, branch: &str) -> Result<Option<gix::ObjectId>> {
    let output = std::process::Command::new("git")
        .arg("ls-remote")
        .arg(url)
        .arg(format!("refs/heads/{branch}"))
        .output()
        .context("failed to execute git ls-remote")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git ls-remote failed for branch `{branch}` at `{url}`: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(sha), Some(r)) = (parts.next(), parts.next())
            && r == format!("refs/heads/{branch}")
        {
            return Ok(Some(
                gix::ObjectId::from_hex(sha.as_bytes())
                    .with_context(|| format!("parse advertised SHA `{sha}`"))?,
            ));
        }
    }
    Ok(None)
}

/// Minimal blocking GitHub API client for update checks (PR listing only).
pub struct GitHub {
    client: reqwest::blocking::Client,
    token: Option<String>,
}

impl GitHub {
    /// Build the client.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(user_agent)
            .build()
            .context("build GitHub API client")?;
        Ok(Self {
            client,
            token: None,
        })
    }

    /// Set the bearer token for authenticated calls.
    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    /// List PRs (open and closed) with the given head in `repo`.
    ///
    /// `head` is a bare branch (same-repo heads) or `owner:branch`.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure or non-success status.
    pub fn prs_for_head(&self, repo: &crate::config::Repo, head: &str) -> Result<Vec<PrSummary>> {
        let mut out = Vec::new();
        let mut url = format!(
            "https://api.github.com/repos/{}/pulls?head={head}&state=all&per_page=100",
            repo.slug()
        );
        loop {
            let mut req = self
                .client
                .get(&url)
                .header("Accept", "application/vnd.github+json");
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            let resp = req
                .send()
                .with_context(|| format!("list PRs with head `{head}` in {}", repo.slug()))?;
            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!(
                    "list PRs with head `{head}` in {} failed: HTTP {status}",
                    repo.slug()
                );
            }
            let next = next_link(resp.headers());
            let page: Vec<serde_json::Value> = resp.json().context("parse PR list response")?;
            out.extend(page.iter().filter_map(|pr| {
                Some(PrSummary {
                    number: pr.get("number")?.as_u64()?,
                    merged: pr.get("merged_at")?.is_string(),
                })
            }));
            match next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(out)
    }
}

/// Extract the `rel="next"` pagination URL, if present.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in link.split(',') {
        let mut halves = part.split(';');
        let url = halves.next()?.trim();
        let rel = halves.next().unwrap_or("").trim();
        if rel.contains("next") {
            return url
                .strip_prefix('<')
                .and_then(|u| u.strip_suffix('>'))
                .map(str::to_string);
        }
    }
    None
}

/// Observe live state for every declared patch: current tip (via ls-remote)
/// plus associated PRs (head match in the patch repo and in the output repo).
///
/// `url_for` maps a repo to its transport URL (auth embedded as needed).
///
/// # Errors
///
/// Returns an error when any transport or API call fails.
pub fn observe_patches(
    github: &GitHub,
    patches: &[crate::config::PatchSpec],
    output: &BranchRef,
    url_for: &dyn Fn(&crate::config::Repo) -> String,
) -> Result<Vec<PatchStatus>> {
    let mut out = Vec::with_capacity(patches.len());
    for spec in patches {
        let patch = &spec.branch;
        let tip = resolve_tip(&url_for(&patch.repo), &patch.branch)?;
        let mut prs = github.prs_for_head(&patch.repo, &patch.branch)?;
        if patch.repo.slug() != output.repo.slug() {
            let cross = format!("{}:{}", patch.repo.owner, patch.branch);
            prs.extend(github.prs_for_head(&output.repo, &cross)?);
        }
        tracing::info!(patch = %patch, tip = ?tip.map(|t| t.to_string()), prs = prs.len(), "observed patch");
        out.push(PatchStatus {
            patch: patch.clone(),
            pin: spec.pin,
            tip,
            prs,
        });
    }
    Ok(out)
}

/// Write the update plan into the caller files: drop removed patches from
/// the config and rewrite the lockfile from surviving tips.
///
/// In dry-run mode, log instead of writing.
///
/// # Errors
///
/// Returns an error when files cannot be read or written.
pub fn apply_to_files(
    config_path: &Path,
    lock_path: &Path,
    cfg: &SynthesisConfig,
    statuses: &[PatchStatus],
    plan: &UpdatePlan,
    dry_run: bool,
) -> Result<()> {
    let mut cfg = cfg.clone();
    for (removed, pr_number) in &plan.removed {
        cfg.patches.retain(|p| {
            p.branch.repo.slug() != removed.repo.slug() || p.branch.branch != removed.branch
        });
        tracing::info!(patch = %removed, pr = pr_number, "proposing patch removal (PR merged)");
    }
    // Lock covers pinned surviving patches only: removed ones drop out by
    // rewrite, unpinned ones were never recorded.
    let surviving: Vec<(BranchRef, gix::ObjectId)> = statuses
        .iter()
        .filter(|s| s.pin)
        .filter(|s| {
            !plan
                .removed
                .iter()
                .any(|(r, _)| r.repo.slug() == s.patch.repo.slug() && r.branch == s.patch.branch)
        })
        .filter_map(|s| s.tip.map(|tip| (s.patch.clone(), tip)))
        .collect();
    let lock = Lockfile::from_resolved(&surviving);

    if dry_run {
        tracing::info!("dry run: would rewrite config and lockfile");
        return Ok(());
    }
    let raw = serde_json::to_string_pretty(&cfg).context("serialize config")?;
    std::fs::write(config_path, format!("{raw}\n"))
        .with_context(|| format!("write config {}", config_path.display()))?;
    crate::lockfile::save(lock_path, &lock)?;
    Ok(())
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

    fn bref(owner: &str, name: &str, branch: &str) -> BranchRef {
        BranchRef {
            repo: repo(owner, name),
            branch: branch.into(),
        }
    }

    fn oid(byte: u8) -> gix::ObjectId {
        gix::ObjectId::from_hex(format!("{byte:02x}").repeat(20).as_bytes()).expect("oid")
    }

    fn status(
        owner: &str,
        name: &str,
        branch: &str,
        tip: Option<gix::ObjectId>,
        prs: Vec<PrSummary>,
    ) -> PatchStatus {
        PatchStatus {
            patch: bref(owner, name, branch),
            pin: true,
            tip,
            prs,
        }
    }

    fn floating(
        owner: &str,
        name: &str,
        branch: &str,
        tip: Option<gix::ObjectId>,
        prs: Vec<PrSummary>,
    ) -> PatchStatus {
        PatchStatus {
            patch: bref(owner, name, branch),
            pin: false,
            tip,
            prs,
        }
    }

    fn pr(number: u64, merged: bool) -> PrSummary {
        PrSummary { number, merged }
    }

    fn pspec(owner: &str, name: &str, branch: &str) -> crate::config::PatchSpec {
        crate::config::PatchSpec {
            branch: bref(owner, name, branch),
            pin: true,
        }
    }

    #[test]
    fn no_changes_empty_plan() {
        let statuses = vec![status("o", "r", "feat", Some(oid(1)), vec![pr(9, false)])];
        let lock = Lockfile::from_resolved(&[(bref("o", "r", "feat"), oid(1))]);
        let plan = plan_updates(&statuses, Some(&lock)).expect("plan");
        assert!(plan.is_empty());
    }

    #[test]
    fn moved_tip_marks_lock_changed() {
        let statuses = vec![status("o", "r", "feat", Some(oid(2)), vec![])];
        let lock = Lockfile::from_resolved(&[(bref("o", "r", "feat"), oid(1))]);
        let plan = plan_updates(&statuses, Some(&lock)).expect("plan");
        assert!(!plan.is_empty());
        assert!(plan.lock_changed);
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn merged_pr_proposes_removal() {
        let statuses = vec![status("o", "r", "feat", Some(oid(1)), vec![pr(12, true)])];
        let lock = Lockfile::from_resolved(&[(bref("o", "r", "feat"), oid(1))]);
        let plan = plan_updates(&statuses, Some(&lock)).expect("plan");
        assert_eq!(plan.removed.len(), 1);
        assert_eq!(plan.removed[0].0, bref("o", "r", "feat"));
        assert_eq!(plan.removed[0].1, 12);
    }

    #[test]
    fn gone_branch_with_merged_pr_proposes_removal() {
        let statuses = vec![status("o", "r", "feat", None, vec![pr(12, true)])];
        let lock = Lockfile::from_resolved(&[(bref("o", "r", "feat"), oid(1))]);
        let plan = plan_updates(&statuses, Some(&lock)).expect("plan");
        assert_eq!(plan.removed.len(), 1);
    }

    #[test]
    fn gone_branch_without_merged_pr_errors() {
        let statuses = vec![status("o", "r", "feat", None, vec![pr(12, false)])];
        let lock = Lockfile::from_resolved(&[(bref("o", "r", "feat"), oid(1))]);
        let err = plan_updates(&statuses, Some(&lock)).expect_err("must fail closed");
        assert!(err.to_string().contains("o/r@feat"), "got: {err}");
    }

    #[test]
    fn no_lockfile_treats_everything_as_changed() {
        let statuses = vec![status("o", "r", "feat", Some(oid(1)), vec![])];
        let plan = plan_updates(&statuses, None).expect("plan");
        assert!(plan.lock_changed);
    }

    #[test]
    fn unpinned_patch_ignored_for_lock_but_not_removal() {
        // A moved floating branch is expected movement, not drift.
        let statuses = vec![floating("o", "r", "own", Some(oid(2)), vec![])];
        let lock = Lockfile::from_resolved(&[(bref("o", "r", "own"), oid(1))]);
        let plan = plan_updates(&statuses, Some(&lock)).expect("plan");
        assert!(plan.is_empty());
        // ...but a merged PR still proposes removal, pinned or not.
        let statuses = vec![floating("o", "r", "own", Some(oid(2)), vec![pr(11, true)])];
        let plan = plan_updates(&statuses, Some(&lock)).expect("plan");
        assert_eq!(plan.removed.len(), 1);
    }

    /// resolve_tip works offline against local bare repos (git CLI).
    #[test]
    fn resolve_tip_reads_local_branches() {
        use gix::objs::tree::EntryKind;
        use gix::refs::transaction::PreviousValue;
        let dir = std::env::temp_dir().join(format!("update-tip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let repo = gix::init_bare(&dir).expect("init bare");
        let sig = gix::actor::SignatureRef::from_bytes(b"t <t@e.c> 1711398853 +0000").expect("sig");
        let blob = repo.write_blob("x").expect("blob");
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        editor
            .upsert("a.txt", EntryKind::Blob, blob.detach())
            .expect("upsert");
        let tree = editor.write().expect("write tree").detach();
        let c = repo
            .new_commit_as(sig, sig, "c", tree, None::<gix::ObjectId>)
            .expect("commit")
            .id;
        repo.reference("refs/heads/feat", c, PreviousValue::Any, "init")
            .expect("ref");

        let url = dir.display().to_string();
        assert_eq!(resolve_tip(&url, "feat").expect("resolve"), Some(c));
        assert_eq!(resolve_tip(&url, "nope").expect("resolve"), None);
    }

    /// apply_to_files rewrites config (minus removed) and lockfile.
    #[test]
    fn apply_rewrites_config_and_lock() {
        use crate::config::SynthesisConfig;
        let dir = std::env::temp_dir().join(format!("update-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let config_path = dir.join("synthesis.json");
        let lock_path = dir.join("synthesis.lock");

        let cfg = SynthesisConfig {
            base: bref("up", "r", "main"),
            patches: vec![pspec("o", "r", "gone"), pspec("o", "r", "kept")],
            output: bref("o", "r", "main"),
            strategy: crate::config::Strategy::Merge,
        };
        let statuses = vec![
            status("o", "r", "gone", Some(oid(9)), vec![pr(3, true)]),
            status("o", "r", "kept", Some(oid(7)), vec![]),
        ];
        let plan = UpdatePlan {
            removed: vec![(bref("o", "r", "gone"), 3)],
            lock_changed: true,
        };
        apply_to_files(&config_path, &lock_path, &cfg, &statuses, &plan, false).expect("apply");

        let back: SynthesisConfig =
            serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read"))
                .expect("parse");
        assert_eq!(back.patches, vec![pspec("o", "r", "kept")]);
        let lock: Lockfile =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).expect("read"))
                .expect("parse");
        assert_eq!(lock.patches.len(), 1);
        assert_eq!(lock.patches[0].branch, "kept");
        assert_eq!(lock.patches[0].sha, oid(7).to_string());
    }

    /// Dry run writes nothing.
    #[test]
    fn dry_run_writes_nothing() {
        use crate::config::SynthesisConfig;
        let dir = std::env::temp_dir().join(format!("update-dry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let config_path = dir.join("synthesis.json");
        let lock_path = dir.join("synthesis.lock");
        let cfg = SynthesisConfig {
            base: bref("up", "r", "main"),
            patches: vec![pspec("o", "r", "gone")],
            output: bref("o", "r", "main"),
            strategy: crate::config::Strategy::Merge,
        };
        let statuses = vec![status("o", "r", "gone", Some(oid(9)), vec![pr(3, true)])];
        let plan = UpdatePlan {
            removed: vec![(bref("o", "r", "gone"), 3)],
            lock_changed: true,
        };
        apply_to_files(&config_path, &lock_path, &cfg, &statuses, &plan, true).expect("dry");
        assert!(!config_path.exists());
        assert!(!lock_path.exists());
    }
}
