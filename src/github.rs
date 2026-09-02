//! GitHub interaction and the stack-discovery seam.
//!
//! # Status
//!
//! The GitHub App identity wiring (JWT from the app private key, installation
//! tokens, webhook verification, `octocrab` client construction) is still a
//! placeholder — see the notes at the bottom of this module.
//!
//! What is implemented and tested here is the **stack-discovery ordering**:
//! turning the raw set of a fork's open PRs (each with its head and base
//! branch) into the *ordered* list of local refs the artifact engine composes.
//! This is pure logic, decoupled from any network call, so it is fully
//! unit-testable — the seam where the octocrab layer will plug in later.
//!
//! # Discovery model
//!
//! The fork's artifact is an ordered stack of branches over the upstream base:
//! the fork-owned branch (bottom), then the open pull requests in dependency
//! order. Each PR is placeable once its base is itself placed — the base is
//! either the upstream base branch or the head of an already-placed PR.

/// A GitHub pull request, reduced to the fields discovery needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    /// Pull request number.
    pub number: u64,
    /// The branch carrying the PR's changes (its head).
    pub head_branch: String,
    /// The branch the PR was opened against (its base).
    pub base_branch: String,
}

/// Errors produced while resolving the PR stack order.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StackError {
    /// A PR targets a base that is neither the upstream base nor the head of
    /// any other PR in the stack, so it cannot be placed.
    #[error(
        "PR #{number} targets base branch `{base}`, which is neither the upstream base \
         nor the head of another PR in the stack"
    )]
    UnknownBase { number: u64, base: String },
    /// The PRs form a dependency cycle (or otherwise can never all be placed).
    #[error("the PR stack contains a cycle or unsatisfiable ordering; no valid order exists")]
    Cycle,
}

/// Resolve the ordered list of local refs to compose for a fork's artifact.
///
/// `upstream_base` is the name of the upstream branch the fork tracks (e.g.
/// `main`). `fork_owned` is the fork-owned branch name, if the fork keeps one —
/// it becomes the bottom layer. `prs` are the fork's open pull requests, in
/// any order.
///
/// Returns the ordered local refs (bottom first): the fork-owned branch as
/// `refs/heads/<fork_owned>` and each PR `n` as `refs/pull/<n>/head`. These are
/// the refs the artifact engine (see `engine::stack::compose`) layers over the
/// upstream mirror.
///
/// Ordering is deterministic: the fork-owned branch first, then PRs resolved
/// topologically by base chain, with same-level PRs (same base) taken in
/// ascending PR-number order. Returns [`StackError::UnknownBase`] if a PR's
/// base can never be satisfied, or [`StackError::Cycle`] if the PRs form a
/// cycle.
pub fn discover_stack(
    upstream_base: &str,
    fork_owned: Option<&str>,
    prs: &[PrInfo],
) -> Result<Vec<String>, StackError> {
    use std::collections::HashSet;

    // Branch names already satisfiable as a base: the upstream base and the
    // fork-owned head (an implicit first layer).
    let mut placed: HashSet<&str> = HashSet::from([upstream_base]);
    if let Some(f) = fork_owned {
        placed.insert(f);
    }

    let mut ordered: Vec<String> = Vec::new();
    if let Some(f) = fork_owned {
        ordered.push(format!("refs/heads/{f}"));
    }

    let mut remaining: Vec<&PrInfo> = prs.iter().collect();

    while !remaining.is_empty() {
        // Among placeable PRs (base already placed), pick the smallest number
        // for deterministic output regardless of input order.
        let next = remaining
            .iter()
            .filter(|p| placed.contains(p.base_branch.as_str()))
            .min_by_key(|p| p.number);

        let Some(p) = next else {
            // No PR is placeable: either a cycle, or a base that can never be
            // satisfied.
            if remaining.iter().all(|p| {
                p.base_branch == upstream_base
                    || prs.iter().any(|q| q.head_branch == p.base_branch)
                    || fork_owned == Some(p.base_branch.as_str())
            }) {
                return Err(StackError::Cycle);
            }
            let offender = remaining[0];
            return Err(StackError::UnknownBase {
                number: offender.number,
                base: offender.base_branch.clone(),
            });
        };

        let idx = remaining
            .iter()
            .position(|r| std::ptr::eq(*r, *p))
            .expect("p is borrowed from remaining");
        let p = remaining.remove(idx);

        placed.insert(&p.head_branch);
        ordered.push(format!("refs/pull/{}/head", p.number));
    }

    Ok(ordered)
}

// ---------------------------------------------------------------------------
// Future wiring (not yet implemented):
// - GitHub App identity: build a JWT from the app's private key, exchange it
//   for a short-lived installation token, and cache/refresh it.
// - A typed `octocrab` client authenticated as the installation.
// - `list_open_pull_requests(fork)` -> Vec<PrInfo>: fetch the fork's open PRs
//   via the API and map each to its head/base branch, then feed
//   `discover_stack`.
// - Webhook signature verification and handler dispatch.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, head: &str, base: &str) -> PrInfo {
        PrInfo {
            number,
            head_branch: head.into(),
            base_branch: base.into(),
        }
    }

    #[test]
    fn orders_fork_owned_then_stacked_prs() {
        // fork-owned is the bottom; PR 12 targets main, PR 13 stacks on PR 12.
        let prs = vec![
            pr(13, "feat-b", "feat-a"),
            pr(12, "feat-a", "main"),
        ];
        let stack = discover_stack("main", Some("fork-owned"), &prs).expect("stack");
        assert_eq!(
            stack,
            vec![
                "refs/heads/fork-owned".to_string(),
                "refs/pull/12/head".to_string(),
                "refs/pull/13/head".to_string(),
            ]
        );
    }

    #[test]
    fn orders_same_level_prs_by_number() {
        // Two independent PRs both targeting main; order by number regardless
        // of input order.
        let prs = vec![pr(20, "b", "main"), pr(19, "a", "main")];
        let stack = discover_stack("main", None, &prs).expect("stack");
        assert_eq!(
            stack,
            vec![
                "refs/pull/19/head".to_string(),
                "refs/pull/20/head".to_string(),
            ]
        );
    }

    #[test]
    fn returns_empty_when_no_prs_and_no_fork_owned() {
        let stack = discover_stack("main", None, &[]).expect("stack");
        assert!(stack.is_empty());
    }

    #[test]
    fn fork_owned_only_when_no_prs() {
        let stack = discover_stack("main", Some("fork-owned"), &[]).expect("stack");
        assert_eq!(stack, vec!["refs/heads/fork-owned".to_string()]);
    }

    #[test]
    fn errors_on_unknown_base() {
        // PR 30 targets "no-such-branch", which is nothing in the stack.
        let prs = vec![pr(30, "x", "no-such-branch")];
        let err = discover_stack("main", None, &prs).expect_err("should error");
        assert_eq!(
            err,
            StackError::UnknownBase {
                number: 30,
                base: "no-such-branch".into()
            }
        );
    }

    #[test]
    fn errors_on_cycle() {
        // PR 40 base = b, PR 41 base = a: a cycle.
        let prs = vec![pr(40, "a", "b"), pr(41, "b", "a")];
        let err = discover_stack("main", None, &prs).expect_err("should error");
        assert_eq!(err, StackError::Cycle);
    }

    #[test]
    fn allows_pr_stacked_on_fork_owned() {
        // fork-owned is a valid base layer; PR 50 stacks directly on it.
        let prs = vec![pr(50, "feat", "fork-owned")];
        let stack = discover_stack("main", Some("fork-owned"), &prs).expect("stack");
        assert_eq!(
            stack,
            vec![
                "refs/heads/fork-owned".to_string(),
                "refs/pull/50/head".to_string(),
            ]
        );
    }
}
