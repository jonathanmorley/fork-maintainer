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

    /// The HTTPS clone/fetch URL of the repository, e.g.
    /// `https://github.com/{owner}/{name}.git`.
    pub fn https_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.name)
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
    /// The fork's default branch name (the knob). Defaults to `main`; this is
    /// the branch whose *name* selects the upstream branch to track and whose
    /// *contents* are the artifact.
    #[serde(default = "default_branch_default")]
    pub default_branch: String,
    /// Local path to the repository (bare mirror) the app syncs and recomposes
    /// for this fork. When absent, the poll loop skips the fork (it has no
    /// working copy to drive).
    #[serde(default)]
    pub local_mirror: Option<String>,
    /// Optionally override the upstream branch to track when it differs from
    /// the fork's default branch name.
    #[serde(default)]
    pub override_upstream_branch: Option<String>,
}

fn default_branch_default() -> String {
    "main".to_string()
}

impl ForkConfig {
    /// The name of the upstream branch to track.
    ///
    /// Defaults to the fork's default branch name (the knob); an explicit
    /// override wins when present.
    pub fn upstream_branch(&self) -> String {
        self.override_upstream_branch
            .clone()
            .unwrap_or_else(|| self.default_branch.clone())
    }

    /// The mirror branch name: `upstream/<X>`.
    pub fn mirror_branch(&self) -> String {
        format!("upstream/{}", self.upstream_branch())
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
            // default_branch defaults to "main"; the knob.
            default_branch: "main".into(),
            local_mirror: None,
            override_upstream_branch: None,
        };
        // The knob: fork default branch "main" tracks upstream/main.
        assert_eq!(cfg.upstream_branch(), "main");
        assert_eq!(cfg.mirror_branch(), "upstream/main");
    }

    #[test]
    fn default_branch_defaults_to_main() {
        // When a config omits default_branch, serde applies the default.
        let json = r#"{
            "upstream": { "owner": "integrations", "name": "terraform-provider-github" },
            "fork": { "owner": "myorg", "name": "terraform-provider-github" }
        }"#;
        let cfg: ForkConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.default_branch, "main");
        assert_eq!(cfg.mirror_branch(), "upstream/main");
    }

    #[test]
    fn mirror_branch_derives_from_any_default_name() {
        let cfg = ForkConfig {
            upstream: repo(),
            fork: repo(),
            // Fork sets its default to "v5" => track upstream/v5.
            default_branch: "v5".into(),
            local_mirror: None,
            override_upstream_branch: None,
        };
        assert_eq!(cfg.upstream_branch(), "v5");
        assert_eq!(cfg.mirror_branch(), "upstream/v5");
    }

    #[test]
    fn override_wins_over_fork_default() {
        let cfg = ForkConfig {
            upstream: repo(),
            fork: repo(),
            default_branch: "main".into(),
            local_mirror: None,
            override_upstream_branch: Some("canonical".into()),
        };
        assert_eq!(cfg.upstream_branch(), "canonical");
        assert_eq!(cfg.mirror_branch(), "upstream/canonical");
    }
}
