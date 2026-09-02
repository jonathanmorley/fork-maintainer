//! fork-maintainer binary entrypoint.
//!
//! When fully wired, this will:
//! 1. Load `AppConfig` from environment / config file.
//! 2. Start an axum webhook server.
//! 3. Run an upstream-drift poll loop.
//!
//! For now it is a minimal scaffold — the engine is tested via the library.

use fork_maintainer::config::ForkConfig;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Placeholder: demonstrate the config module is wired.
    let sample = ForkConfig {
        upstream: fork_maintainer::config::Repo {
            owner: "integrations".into(),
            name: "terraform-provider-github".into(),
        },
        fork: fork_maintainer::config::Repo {
            owner: "jonathanmorley".into(),
            name: "terraform-provider-github".into(),
        },
        override_upstream_branch: None,
    };

    tracing::info!(upstream = %sample.upstream, fork = %sample.fork, "fork-maintainer starting");
    tracing::info!(mirror = %sample.mirror_branch("main"), "mirror branch derived");
    Ok(())
}
