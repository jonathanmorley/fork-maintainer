//! fork-maintainer binary entrypoint.
//!
//! Loads the app configuration and starts the GitHub webhook server. GitHub
//! POSTs fork events (push, pull_request) to `/api/webhook`; the handler
//! verifies the HMAC signature and dispatches a reconcile for the affected
//! fork.
//!
//! Alongside the webhook, a background poll loop periodically reconciles every
//! configured fork so an idle fork still picks up *upstream* drift (the app
//! cannot subscribe to upstream — webhooks only fire for the repos it is
//! installed on).
//!
//! Configuration comes from `FORK_MAINTAINER_CONFIG` (a JSON file) or
//! `config.json` in the working directory, or defaults to an empty list of
//! forks. See [`fork_maintainer::config::AppConfig`].

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use fork_maintainer::config::{AppConfig, ForkConfig};
use fork_maintainer::github::auth::AppCredentials;
use fork_maintainer::poll::PollOutcome;
use fork_maintainer::webhook::AppState;
use gix::actor::SignatureRef;

/// The committer identity stamped on recompose commits.
const COMMITTER: &[u8] =
    b"fork-maintainer <fork-maintainer@users.noreply.github.com> 1711398853 +0000";

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let cfg = load_config()?;
    tracing::info!(forks = %cfg.forks.len(), "loaded configuration");

    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let app = cfg.credentials();
        let state = AppState {
            secret: cfg.webhook_secret.clone(),
            handle: make_dispatcher(app.clone(), cfg.forks.clone()),
        };
        let router = fork_maintainer::webhook::router(state);

        // Background poll loop for upstream drift; runs the webhook server
        // until it shuts down.
        let poll_handle = spawn_poll_loop(cfg.clone());

        let addr =
            std::env::var("FORK_MAINTAINER_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind webhook listener on {addr}"))?;
        tracing::info!(%addr, "webhook server listening on /api/webhook");
        axum::serve(listener, router)
            .await
            .context("serve webhook")?;

        poll_handle.abort();
        Ok(())
    })
}

/// Spawn the background poll loop.
///
/// Runs a reconcile pass for every configured fork at a fixed interval
/// (default 300s, overridable via `FORK_MAINTAINER_POLL_INTERVAL` seconds).
/// The loop runs until the returned handle is aborted.
fn spawn_poll_loop(cfg: AppConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = std::env::var("FORK_MAINTAINER_POLL_INTERVAL")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(300));
        tracing::info!(seconds = interval.as_secs(), "poll loop started");

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            poll_once(&cfg).await;
        }
    })
}

/// Run a single poll pass for all configured forks.
async fn poll_once(cfg: &AppConfig) {
    let app = cfg.credentials();
    let forks = cfg.forks.clone();
    for fork in &forks {
        let outcome = reconcile_fork(app.clone(), fork.clone()).await;
        log_outcome(fork, outcome);
    }
}

/// Reconcile a single fork against live GitHub, returning its [`PollOutcome`].
///
/// Requires app credentials (to mint an installation client + token) and a
/// configured `local_mirror`. Missing either yields [`PollOutcome::Failed`].
async fn reconcile_fork(app: Option<AppCredentials>, fork: ForkConfig) -> PollOutcome {
    let Some(app) = app else {
        tracing::warn!(fork = %fork.fork, "no app credentials configured; cannot reconcile live");
        return PollOutcome::Failed("no app credentials configured".into());
    };
    if fork.local_mirror.is_none() {
        tracing::warn!(fork = %fork.fork, "fork has no local_mirror; skipping");
        return PollOutcome::Failed("fork has no local_mirror configured".into());
    }
    let committer = SignatureRef::from_bytes(COMMITTER).expect("valid committer");
    let result = fork_maintainer::reconcile::reconcile_and_push_live(&app, &fork, committer).await;
    match result {
        Ok((outcome, push)) => {
            if let Some(push) = push {
                tracing::info!(
                    fork = %fork.fork,
                    pushed = ?push.pushed,
                    "pushed recomposed artifact to fork"
                );
            }
            fork_maintainer::poll::classify(Ok(outcome))
        }
        Err(e) => fork_maintainer::poll::classify(Err(e)),
    }
}

/// Log a fork's poll outcome at the appropriate level.
fn log_outcome(fork: &ForkConfig, outcome: PollOutcome) {
    match outcome {
        PollOutcome::NoChange => tracing::debug!(fork = %fork.fork, "no upstream drift"),
        PollOutcome::Changed { note } => {
            tracing::info!(fork = %fork.fork, %note, "fork reconciled")
        }
        PollOutcome::Failed(err) => tracing::warn!(fork = %fork.fork, "reconcile failed: {err}"),
    }
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
        private_key_pem: String::new(),
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
/// On a valid event for a configured fork, spawns a background reconcile of
/// that fork (live discovery + full engine). Fork events fire only for repos
/// the app is installed on, so matching the payload's `owner/name` against the
/// configured forks is the right gate.
fn make_dispatcher(
    app: Option<AppCredentials>,
    forks: Vec<ForkConfig>,
) -> impl Fn(String) + Send + Sync + Clone + 'static {
    move |full_name: String| {
        let Some(fork) = forks.iter().find(|f| f.fork.slug() == full_name).cloned() else {
            tracing::warn!(fork = %full_name, "event for unconfigured fork; ignored");
            return;
        };
        tracing::info!(fork = %full_name, "reconcile requested for fork");
        let app = app.clone();
        tokio::spawn(async move {
            let outcome = crate::reconcile_fork(app, fork.clone()).await;
            crate::log_outcome(&fork, outcome);
        });
    }
}
