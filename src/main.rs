//! fork-maintainer binary entrypoint.
//!
//! Loads the app configuration and starts the GitHub webhook server. GitHub
//! POSTs fork events (push, pull_request) to `/api/webhook`; the handler
//! verifies the HMAC signature and dispatches a reconcile for the affected
//! fork.
//!
//! Configuration comes from `FORK_MAINTAINER_CONFIG` (a JSON file) or
//! `config.json` in the working directory, or defaults to an empty list of
//! forks. See [`fork_maintainer::config::AppConfig`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use fork_maintainer::config::{AppConfig, ForkConfig};
use fork_maintainer::webhook::AppState;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let cfg = load_config()?;
    tracing::info!(forks = %cfg.forks.len(), "loaded configuration");

    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let state = AppState {
            secret: cfg.webhook_secret.clone(),
            handle: make_dispatcher(cfg.forks.clone()),
        };
        let router = fork_maintainer::webhook::router(state);

        let addr =
            std::env::var("FORK_MAINTAINER_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind webhook listener on {addr}"))?;
        tracing::info!(%addr, "webhook server listening on /api/webhook");
        axum::serve(listener, router)
            .await
            .context("serve webhook")?;
        Ok(())
    })
}

/// Load `AppConfig` from `FORK_MAINTAINER_CONFIG`, falling back to
/// `config.json`, or an empty config if neither exists.
fn load_config() -> Result<AppConfig> {
    let path = config_path();
    if let Some(path) = &path {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read config file {}", path.display()))?;
        let cfg: AppConfig = serde_json::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        tracing::info!(path = %path.display(), "loaded config");
        return Ok(cfg);
    }
    tracing::warn!("no config file found; starting with an empty fork list");
    Ok(AppConfig {
        app_id: 0,
        webhook_secret: String::new(),
        forks: vec![],
    })
}

/// Resolve the config file path: `FORK_MAINTAINER_CONFIG`, else `config.json`.
fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FORK_MAINTAINER_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let local = std::path::Path::new("config.json");
    local.exists().then(|| local.to_path_buf())
}

/// Build the reconcile dispatcher for the webhook.
///
/// On a valid event, looks up the affected fork (by `owner/name`) among the
/// configured forks and logs the reconcile request. Full engine execution —
/// resolving the fork's local mirror, minting an install token, fetching PR
/// heads, and running `reconcile` — is wired here once the local-repo layout is
/// configured; for now the fork resolution and event path are exercised.
fn make_dispatcher(forks: Vec<ForkConfig>) -> impl Fn(String) + Send + Sync + Clone + 'static {
    move |full_name: String| match forks.iter().find(|f| f.fork.slug() == full_name) {
        Some(f) => tracing::info!(
            fork = %full_name,
            upstream = %f.upstream,
            "reconcile requested for fork",
        ),
        None => tracing::warn!(fork = %full_name, "event for unconfigured fork; ignored"),
    }
}
