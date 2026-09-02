//! Application configuration.
//!
//! The per-fork configuration model is deliberately minimal:
//! - `upstream` — the repository the fork tracks.
//! - `fork` — the fork itself.
//!
//! Everything else is *derived* from the fork's **default branch name** (the
//! knob) and the upstream repository:
//! - `base` — the name of the upstream branch to track, which is taken to be
//!   the fork's default branch name (`X`).
//! - `mirror` — `upstream/<X>`, the fast-forward-only stack base.
//! - `artifact` — `X`, the fork's own default branch.
//!
//! An optional `override` allows pointing at an upstream branch whose name
//! differs from the fork's default branch name (the 5% case).

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
    /// The `owner/name` slug used in GitHub API paths.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A fork relationship the app maintains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkConfig {
    /// The upstream repository that `fork` tracks.
    pub upstream: Repo,
    /// The fork repository being maintained.
    pub fork: Repo,
    /// Optionally override the upstream branch to track when it differs from
    /// the fork's default branch name.
    #[serde(default)]
    pub override_upstream_branch: Option<String>,
}

impl ForkConfig {
    /// The name of the upstream branch to track.
    ///
    /// Defaults to the fork's default branch name (the knob); an explicit
    /// override wins when present.
    pub fn upstream_branch(&self, fork_default_branch: &str) -> String {
        self.override_upstream_branch
            .clone()
            .unwrap_or_else(|| fork_default_branch.to_string())
    }

    /// The mirror branch name: `upstream/<X>`.
    pub fn mirror_branch(&self, fork_default_branch: &str) -> String {
        format!("upstream/{}", self.upstream_branch(fork_default_branch))
    }
}

/// Application-level settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// GitHub App id.
    pub app_id: u64,
    /// GitHub App webhook secret used to verify webhook payload signatures.
    pub webhook_secret: String,
    /// The forks this app maintains.
    #[serde(default)]
    pub forks: Vec<ForkConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        Repo {
            owner: "integrations".into(),
            name: "terraform-provider-github".into(),
        }
    }

    #[test]
    fn mirror_branch_uses_fork_default_by_default() {
        let cfg = ForkConfig {
            upstream: repo(),
            fork: repo(),
            override_upstream_branch: None,
        };
        // The knob: fork default branch "main" tracks upstream/main.
        assert_eq!(cfg.upstream_branch("main"), "main");
        assert_eq!(cfg.mirror_branch("main"), "upstream/main");
    }

    #[test]
    fn mirror_branch_derives_from_any_default_name() {
        let cfg = ForkConfig {
            upstream: repo(),
            fork: repo(),
            override_upstream_branch: None,
        };
        // Fork sets its default to "v5" => track upstream/v5.
        assert_eq!(cfg.upstream_branch("v5"), "v5");
        assert_eq!(cfg.mirror_branch("v5"), "upstream/v5");
    }

    #[test]
    fn override_wins_over_fork_default() {
        let cfg = ForkConfig {
            upstream: repo(),
            fork: repo(),
            override_upstream_branch: Some("canonical".into()),
        };
        assert_eq!(cfg.upstream_branch("main"), "canonical");
        assert_eq!(cfg.mirror_branch("main"), "upstream/canonical");
    }
}
