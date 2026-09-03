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
///
/// The name and email are fixed; the timestamp is set dynamically when the
/// function is called.
const COMMITTER_IDENTITY: &[u8] = b"fork-maintainer <fork-maintainer@users.noreply.github.com> ";

/// Shared, reusable string buffer for building committer signatures. Reused
/// across calls so we only ever leak a single allocation (the `SignatureRef`
/// needs a `'static` borrow we cannot otherwise produce without an arena).
static COMMITTER_BUFFER: std::sync::OnceLock<std::sync::Mutex<Vec<u8>>> =
    std::sync::OnceLock::new();

/// Build a committer signature with the current time.
fn current_committer() -> Result<SignatureRef<'static>> {
    let buf = COMMITTER_BUFFER.get_or_init(|| {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(COMMITTER_IDENTITY);
        std::sync::Mutex::new(buf)
    });
    let mut buf = buf.lock().expect("committer mutex poisoned");
    // Reset to just the identity, then append the fresh timestamp.
    buf.truncate(COMMITTER_IDENTITY.len());
    let timestamp = chrono::Utc::now().format("%s %z").to_string();
    buf.extend_from_slice(timestamp.as_bytes());
    // Take a snapshot that can live for 'static. The shared buffer is reused,
    // so the per-call snapshot is the only allocation that is intentionally
    // leaked (small and bounded by the number of in-flight reconciles).
    let leaked: &'static [u8] = Box::leak(buf.clone().into_boxed_slice());
    SignatureRef::from_bytes(leaked).context("build committer signature")
}

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
    let committer = match current_committer() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(fork = %fork.fork, "failed to build committer: {e}");
            return PollOutcome::Failed("failed to build committer".into());
        }
    };
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
        validate_config(&cfg, path)?;
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

/// Validate a loaded app config and return useful errors early.
///
/// - Each fork must specify both `upstream` and `fork` (a fork with an
///   incomplete identity is a hard error).
/// - A fork without `local_mirror` is logged as a warning, not a hard error —
///   the poll loop and webhook already skip such forks, so refusing to start
///   the whole app would be a regression.
/// - The app config should have a non-zero `app_id` when a private key is set.
fn validate_config(cfg: &AppConfig, path: &std::path::Path) -> Result<()> {
    for fork in &cfg.forks {
        if fork.local_mirror.is_none() {
            tracing::warn!(
                path = %path.display(),
                fork = %fork.fork.slug(),
                "fork has no `local_mirror`; the poll loop and webhook will skip it"
            );
        }
        if fork.upstream.owner.is_empty() || fork.upstream.name.is_empty() {
            anyhow::bail!(
                "{}: fork `{}` has an incomplete `upstream`",
                path.display(),
                fork.fork.slug()
            );
        }
        if fork.fork.owner.is_empty() || fork.fork.name.is_empty() {
            anyhow::bail!("{}: fork has an incomplete `fork` identity", path.display());
        }
    }
    if !cfg.private_key_pem.is_empty() && cfg.app_id == 0 {
        anyhow::bail!(
            "{}: `app_id` must be non-zero when `private_key_pem` is set",
            path.display()
        );
    }
    if cfg.app_id != 0 && cfg.private_key_pem.is_empty() {
        tracing::warn!(
            "{}: `app_id` is set but `private_key_pem` is empty; live reconcile will be unavailable",
            path.display()
        );
    }
    Ok(())
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
