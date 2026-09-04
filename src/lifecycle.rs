//! Post-synthesis lifecycle — tidy branches whose PRs merged.
//!
//! After a green synthesis, a patch whose associated pull request is merged
//! is dead weight: its content reached its destination through the merge,
//! and the branch only lingers. v1 automates exactly this one mechanical
//! rule — nothing semantic (no "superseded another way" detection, no PR
//! closing, no archiving).
//!
//! The decision ([`plan`]) is pure and fully unit-tested; the GitHub calls
//! ([`GitHub`]) are a thin blocking layer over two REST endpoints. Opt-in
//! via `--lifecycle delete-merged` (default off), with `--dry-run` logging
//! what would happen.

use anyhow::{Context, Result};

use crate::config::{BranchRef, Repo};

/// A pull request reduced to what lifecycle decisions need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    /// Pull request number (for log lines).
    pub number: u64,
    /// True when the PR was merged (as opposed to closed unmerged).
    pub merged: bool,
}

/// A patch branch plus the PRs associated with it (by head branch, in the
/// patch's own repo and in the output repo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPrs {
    /// The patch branch.
    pub patch: BranchRef,
    /// Associated PRs, if any.
    pub prs: Vec<PrSummary>,
}

/// A lifecycle mutation to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Delete a fully-merged patch branch.
    DeleteBranch {
        repo: Repo,
        branch: String,
        pr_number: u64,
    },
}

/// Decide lifecycle actions for patches.
///
/// Every patch with at least one merged associated PR yields a
/// [`LifecycleAction::DeleteBranch`]. The base and output branches are never
/// deleted, even if a PR record claims a merge — synthesis inputs and outputs
/// are not tidy targets.
pub fn plan(patches: &[PatchPrs], base: &BranchRef, output: &BranchRef) -> Vec<LifecycleAction> {
    let protected = [
        (base.repo.slug(), base.branch.clone()),
        (output.repo.slug(), output.branch.clone()),
    ];
    let mut actions = Vec::new();
    for patch in patches {
        let key = (patch.patch.repo.slug(), patch.patch.branch.clone());
        if protected.contains(&key) {
            continue;
        }
        if let Some(pr) = patch.prs.iter().find(|pr| pr.merged) {
            actions.push(LifecycleAction::DeleteBranch {
                repo: patch.patch.repo.clone(),
                branch: patch.patch.branch.clone(),
                pr_number: pr.number,
            });
        }
    }
    actions
}

/// Minimal blocking GitHub API client for lifecycle calls.
///
/// Only the two endpoints v1 needs: listing PRs by head branch and deleting
/// a ref. Authenticated with a bearer token when one is configured.
pub struct GitHub {
    client: reqwest::blocking::Client,
    token: Option<String>,
}

impl GitHub {
    /// Build the client. `user_agent` identifies the app to the API.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built (TLS backend).
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

    fn get(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        let req = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// List PRs (open and closed) with the given head branch in `repo`.
    ///
    /// `head` is either a bare branch (same-repo heads) or `owner:branch`
    /// (cross-repo heads), per the API's `head` filter.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure or non-success status.
    pub fn prs_for_head(&self, repo: &Repo, head: &str) -> Result<Vec<PrSummary>> {
        let mut out = Vec::new();
        let mut url = format!(
            "https://api.github.com/repos/{}/pulls?head={head}&state=all&per_page=100",
            repo.slug(),
        );
        loop {
            let resp = self
                .get(&url)
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

    /// Delete a branch by removing its ref.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure or non-success status
    /// (including protected-branch rejections).
    pub fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/git/refs/heads/{branch}",
            repo.slug()
        );
        let mut req = self
            .client
            .delete(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .with_context(|| format!("delete branch `{branch}` in {}", repo.slug()))?;
        let status = resp.status();
        if status != reqwest::StatusCode::NO_CONTENT {
            anyhow::bail!(
                "delete branch `{branch}` in {} failed: HTTP {status}",
                repo.slug()
            );
        }
        Ok(())
    }
}

/// Extract the `rel="next"` pagination URL from a Link header, if present.
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

/// Collect associated PRs for every patch: PRs with the patch's branch as
/// head in the patch's own repo, plus PRs with `owner:branch` head in the
/// output repo (the cross-repo case: PR opened against the output).
///
/// # Errors
///
/// Returns an error when any listing call fails.
pub fn collect_patch_prs(
    github: &GitHub,
    patches: &[BranchRef],
    output: &BranchRef,
) -> Result<Vec<PatchPrs>> {
    let mut out = Vec::with_capacity(patches.len());
    for patch in patches {
        let mut prs = github.prs_for_head(&patch.repo, &patch.branch)?;
        // Cross-repo heads targeting the output repo (skip when the patch
        // already lives there — same query, no need to repeat).
        if patch.repo.slug() != output.repo.slug() {
            let cross = format!("{}:{}", patch.repo.owner, patch.branch);
            prs.extend(github.prs_for_head(&output.repo, &cross)?);
        }
        out.push(PatchPrs {
            patch: patch.clone(),
            prs,
        });
    }
    Ok(out)
}

/// Apply planned actions: delete each branch, or log it under `dry_run`.
///
/// # Errors
///
/// Returns an error when any deletion fails.
pub fn apply(github: &GitHub, actions: &[LifecycleAction], dry_run: bool) -> Result<()> {
    for action in actions {
        match action {
            LifecycleAction::DeleteBranch {
                repo,
                branch,
                pr_number,
            } => {
                if dry_run {
                    tracing::info!(
                        repo = %repo.slug(),
                        branch = %branch,
                        pr = pr_number,
                        "dry run: would delete merged patch branch"
                    );
                    continue;
                }
                tracing::info!(
                    repo = %repo.slug(),
                    branch = %branch,
                    pr = pr_number,
                    "deleting merged patch branch"
                );
                github.delete_branch(repo, branch)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn pr(number: u64, merged: bool) -> PrSummary {
        PrSummary { number, merged }
    }

    fn base_and_output() -> (BranchRef, BranchRef) {
        (branch("up", "repo", "main"), branch("me", "repo", "main"))
    }

    #[test]
    fn merged_pr_plans_branch_deletion() {
        let (base, output) = base_and_output();
        let patches = vec![PatchPrs {
            patch: branch("me", "repo", "feature"),
            prs: vec![pr(12, true)],
        }];
        assert_eq!(
            plan(&patches, &base, &output),
            vec![LifecycleAction::DeleteBranch {
                repo: repo("me", "repo"),
                branch: "feature".into(),
                pr_number: 12,
            }]
        );
    }

    #[test]
    fn open_or_absent_prs_plan_nothing() {
        let (base, output) = base_and_output();
        let patches = vec![
            PatchPrs {
                patch: branch("me", "repo", "wip"),
                prs: vec![pr(9, false)],
            },
            PatchPrs {
                patch: branch("me", "repo", "lonely"),
                prs: vec![],
            },
        ];
        assert!(plan(&patches, &base, &output).is_empty());
    }

    #[test]
    fn closed_unmerged_pr_plans_nothing() {
        // merged_at null (closed without merge) must not delete.
        let (base, output) = base_and_output();
        let patches = vec![PatchPrs {
            patch: branch("me", "repo", "abandoned"),
            prs: vec![pr(7, false)],
        }];
        assert!(plan(&patches, &base, &output).is_empty());
    }

    #[test]
    fn base_and_output_branches_are_never_deleted() {
        let (base, output) = base_and_output();
        let patches = vec![
            PatchPrs {
                patch: base.clone(),
                prs: vec![pr(1, true)],
            },
            PatchPrs {
                patch: output.clone(),
                prs: vec![pr(2, true)],
            },
        ];
        assert!(plan(&patches, &base, &output).is_empty());
    }

    #[test]
    fn mixed_patches_plan_only_merged() {
        let (base, output) = base_and_output();
        let patches = vec![
            PatchPrs {
                patch: branch("me", "repo", "done"),
                prs: vec![pr(3, false), pr(4, true)],
            },
            PatchPrs {
                patch: branch("you", "repo", "wip"),
                prs: vec![pr(5, false)],
            },
        ];
        let actions = plan(&patches, &base, &output);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            LifecycleAction::DeleteBranch { pr_number: 4, .. }
        ));
    }

    #[test]
    fn next_link_parses_rfc5988() {
        use reqwest::header::HeaderMap;
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            r#"<https://api.github.com/repos/o/r/pulls?page=2>; rel="next", <https://api.github.com/repos/o/r/pulls?page=5>; rel="last""#
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_link(&headers).as_deref(),
            Some("https://api.github.com/repos/o/r/pulls?page=2")
        );
    }

    #[test]
    fn next_link_absent_without_header() {
        assert_eq!(next_link(&reqwest::header::HeaderMap::new()), None);
    }
}
