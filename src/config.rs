//! Synthesis configuration.
//!
//! The declarative spec: a base branch plus an ordered set of patches,
//! producing one synthesized output branch.
//!
//! ```json
//! {
//!   "base": { "repo": { "owner": "integrations", "name": "repo" }, "branch": "main" },
//!   "patches": [
//!     { "repo": { "owner": "myorg", "name": "repo" }, "branch": "fork-owned" },
//!     { "repo": { "owner": "other", "name": "repo" }, "branch": "feature-x" }
//!   ],
//!   "output": { "repo": { "owner": "myorg", "name": "repo" }, "branch": "main" },
//!   "strategy": "merge"
//! }
//! ```
//!
//! Patches may live in **any repository** the token can read — there is no
//! same-repo requirement. Each patch is fetched independently from its own
//! repo URL; composition only ever sees trees.

use serde::{Deserialize, Serialize};

/// Repository identity as `{owner}/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    /// The owner (user or organization).
    pub owner: String,
    /// The repository name.
    pub name: String,
}

impl Repo {
    /// The `owner/name` slug used in refspecs and log lines.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The HTTPS clone/fetch URL of the repository, e.g.
    /// `https://github.com/{owner}/{name}.git`.
    pub fn https_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.name)
    }

    /// The HTTPS URL authenticated as `x-access-token`, for git operations
    /// with a short-lived token (a PAT, or a GitHub App installation token).
    pub fn authed_https_url(&self, token: &str) -> String {
        format!(
            "https://x-access-token:{token}@github.com/{}/{}.git",
            self.owner, self.name
        )
    }
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A branch in a repository: the unit of base, patch, and output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRef {
    /// The repository holding the branch.
    pub repo: Repo,
    /// The branch name (e.g. `main`, `fork-owned`).
    pub branch: String,
}

impl BranchRef {
    /// Compact `owner/name@branch` form used by the CLI and workflow inputs.
    pub fn compact(&self) -> String {
        format!("{}@{}", self.repo.slug(), self.branch)
    }

    /// Parse the compact `owner/name@branch` form.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not `owner/name@branch` shaped.
    pub fn parse_compact(value: &str) -> anyhow::Result<Self> {
        let (repo_part, branch) = value
            .rsplit_once('@')
            .ok_or_else(|| anyhow::anyhow!("expected `owner/name@branch`, got `{value}`"))?;
        let (owner, name) = repo_part
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("expected `owner/name@branch`, got `{value}`"))?;
        if owner.is_empty() || name.is_empty() || branch.is_empty() {
            anyhow::bail!("expected `owner/name@branch`, got `{value}`");
        }
        Ok(Self {
            repo: Repo {
                owner: owner.to_string(),
                name: name.to_string(),
            },
            branch: branch.to_string(),
        })
    }
}

impl std::fmt::Display for BranchRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.compact())
    }
}

/// How patch layers are applied onto the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Tree overlay: last-write-wins per path. Never fails on conflicts, but
    /// can silently drop overlapping changes. Availability over correctness.
    Overlay,
    /// Three-way merge per layer with conflict detection. A conflict fails
    /// the run before anything is pushed. Correctness over availability.
    #[default]
    Merge,
}

impl Strategy {
    /// Parse a strategy name; valid values are `overlay` and `merge`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the valid values for anything else.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name {
            "overlay" => Ok(Self::Overlay),
            "merge" => Ok(Self::Merge),
            other => anyhow::bail!("unknown strategy `{other}` (valid: overlay, merge)"),
        }
    }
}

/// The full declarative synthesis spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisConfig {
    /// The base branch the artifact is rebuilt from every run.
    pub base: BranchRef,
    /// Ordered patch branches, applied bottom-first onto the base.
    #[serde(default)]
    pub patches: Vec<BranchRef>,
    /// Where the synthesized commit is force-pushed.
    pub output: BranchRef,
    /// Composition strategy. Defaults to `merge`.
    #[serde(default)]
    pub strategy: Strategy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls() {
        let r = Repo {
            owner: "integrations".into(),
            name: "repo".into(),
        };
        assert_eq!(r.slug(), "integrations/repo");
        assert_eq!(r.https_url(), "https://github.com/integrations/repo.git");
        assert_eq!(
            r.authed_https_url("tok"),
            "https://x-access-token:tok@github.com/integrations/repo.git"
        );
    }

    #[test]
    fn compact_round_trip() {
        let b = BranchRef {
            repo: Repo {
                owner: "myorg".into(),
                name: "repo".into(),
            },
            branch: "fork-owned".into(),
        };
        assert_eq!(b.compact(), "myorg/repo@fork-owned");
        assert_eq!(
            BranchRef::parse_compact("myorg/repo@fork-owned").unwrap(),
            b
        );
    }

    #[test]
    fn compact_rejects_malformed() {
        for bad in ["", "main", "owner/repo", "owner/repo@", "@main", "/@"] {
            assert!(
                BranchRef::parse_compact(bad).is_err(),
                "should reject `{bad}`"
            );
        }
    }

    #[test]
    fn strategy_parses_known_names() {
        assert_eq!(Strategy::parse("overlay").unwrap(), Strategy::Overlay);
        assert_eq!(Strategy::parse("merge").unwrap(), Strategy::Merge);
        assert!(Strategy::parse("rebase").is_err());
    }

    #[test]
    fn strategy_defaults_to_merge() {
        let json = r#"{
            "base": {"repo": {"owner": "u", "name": "r"}, "branch": "main"},
            "output": {"repo": {"owner": "f", "name": "r"}, "branch": "main"}
        }"#;
        let cfg: SynthesisConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.strategy, Strategy::Merge);
        assert!(cfg.patches.is_empty());
    }
}
