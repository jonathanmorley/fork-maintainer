//! GitHub webhook handling: signature verification and event dispatch.
//!
//! GitHub POSTs webhook payloads to a configured URL with two headers the
//! receiver must trust:
//!
//! - `X-Hub-Signature-256`: `sha256=<hex HMAC-SHA256 of the raw body>` keyed by
//!   the app's webhook secret. This proves the payload came from GitHub and was
//!   not tampered with in transit.
//! - `X-GitHub-Event`: the event name (e.g. `push`, `pull_request`).
//!
//! The core of this module — signature verification and event classification —
//! is pure and fully unit-testable with no network. The [`handler`] adapts it
//! to an axum endpoint.
//!
//! # Security
//!
//! Signature verification **must** run before any payload-driven work. It uses
//! a constant-time comparison to avoid leaking timing information about the
//! secret.

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The header GitHub uses to send the webhook signature.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";
/// The header carrying the event name (e.g. `push`, `pull_request`).
pub const EVENT_HEADER: &str = "x-github-event";
/// The signature header prefix; the hex digest follows.
pub const SIGNATURE_PREFIX: &str = "sha256=";

type HmacSha256 = Hmac<Sha256>;

/// Compute the hex HMAC-SHA256 of `body` keyed by `secret`.
fn hmac_hex(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Verify that `header` (the `X-Hub-Signature-256` value) matches `body`.
///
/// The header is expected to be `sha256=<hex digest>`. Returns `false` if the
/// header is missing/empty, not `sha256=`-prefixed, or the digest does not
/// match a constant-time comparison against the body's HMAC keyed by `secret`.
///
/// # Security
///
/// This uses constant-time comparison, so an attacker cannot learn the secret
/// by measuring response timing on partial matches.
pub fn verify_signature(secret: &str, header: Option<&str>, body: &[u8]) -> bool {
    let Some(value) = header else {
        return false;
    };
    let Some(hex_digest) = value.strip_prefix(SIGNATURE_PREFIX) else {
        return false;
    };
    // GitHub sends 64 lowercase hex chars; anything else cannot match.
    if hex_digest.len() != 64 {
        return false;
    }
    let expected = hmac_hex(secret, body);
    // Constant-time comparison of the two hex strings.
    constant_time_eq(expected.as_bytes(), hex_digest.as_bytes())
}

/// Constant-time byte-string equality (no early exit on mismatch).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // XOR every byte together; only equal strings yield 0.
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A GitHub webhook event, reduced to the parts that drive reconcile.
///
/// Only events that can change the fork's artifact are interesting; everything
/// else is ignored by the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventName {
    /// A push to a branch (e.g. the fork's default branch, or a PR head).
    Push,
    /// A pull request event: opened, reopened, synchronized, etc.
    PullRequest,
}

impl EventName {
    /// Map GitHub's event-name string to the typed form.
    ///
    /// Returns `None` for events the app does not act on (e.g. `issues`,
    /// `star`, `workflow_run`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "push" => Some(Self::Push),
            "pull_request" => Some(Self::PullRequest),
            _ => None,
        }
    }
}

/// Minimal projection of a webhook payload: the owning repo's full name.
///
/// Webhook payloads vary, but every fork-relevant one carries the repository
/// it belongs to (`repository.full_name`, e.g. `myorg/terraform-provider-github`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Payload {
    pub repository: PayloadRepository,
}

/// The repository portion of a webhook payload.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PayloadRepository {
    pub full_name: String,
}

/// The decision a webhook handler reaches, fully resolved and decoupled from
/// the network so it can be reasoned about (and tested) in isolation.
///
/// [`dispatch`] produces this from the request triple (event name, signature
/// header, body); callers act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The signature did not verify; the request must be rejected (401).
    BadSignature,
    /// The event is not one we act on; ignore (200).
    Ignored,
    /// The payload was not valid JSON / lacked a repository (400).
    BadPayload(String),
    /// Reconcile the fork at `full_name` (200).
    Reconcile { fork: String },
}

/// Classify a webhook request into a [`Decision`].
///
/// This is the pure core the axum [`handler`] calls. It verifies the signature,
/// extracts the event name, and — for acting events — resolves which repository
/// to reconcile.
///
/// # Errors
///
/// Returns `Err` only for IO-level surprises (e.g. malformed JSON that serde
/// cannot project); policy outcomes are [`Decision`] variants, not errors.
pub fn dispatch(
    secret: &str,
    event_name: Option<&str>,
    signature: Option<&str>,
    body: &[u8],
) -> Result<Decision> {
    // Security gate: reject anything that fails signature verification, before
    // even looking at the event or payload.
    if !verify_signature(secret, signature, body) {
        return Ok(Decision::BadSignature);
    }

    // Only events that can change the artifact proceed; others are ignored.
    if event_name.and_then(EventName::from_name).is_none() {
        return Ok(Decision::Ignored);
    }

    // Determine the fork by projecting the payload's repository.
    let payload: Payload = serde_json::from_slice(body).map_err(|e| anyhow::anyhow!(e))?;
    Ok(Decision::Reconcile {
        fork: payload.repository.full_name,
    })
}

/// The application state the webhook handler needs.
///
/// `secret` is the configured webhook secret. `handle` is invoked for a valid
/// [`Decision::Reconcile`] — the seam where the caller wires the engine (find
/// the fork's local mirror, run `reconcile`). It is a boxed async closure so
/// the handler stays free of octocrab/engine specifics and is trivially
/// testable with a recording closure.
#[derive(Clone)]
pub struct AppState<F> {
    /// GitHub App webhook secret.
    pub secret: String,
    /// Called with the fork `owner/name` to reconcile after a valid event.
    pub handle: F,
}

/// The axum handler for the webhook endpoint (POST `/api/webhook`).
///
/// Reads the raw body and headers, defers to [`dispatch`], and maps the
/// resulting [`Decision`] to an HTTP response:
///
/// - [`Decision::BadSignature`] → `401 Unauthorized`
/// - [`Decision::BadPayload`] → `400 Bad Request`
/// - [`Decision::Ignored`] → `200 OK` (acknowledge, do nothing)
/// - [`Decision::Reconcile`] → invoke `state.handle`, then `200 OK`
pub async fn handler<F>(
    State(state): State<AppState<F>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::http::StatusCode
where
    F: Fn(String) + Send + Sync + Clone + 'static,
{
    let event = headers
        .get(EVENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    match dispatch(&state.secret, event.as_deref(), signature.as_deref(), &body) {
        Ok(Decision::Reconcile { fork }) => {
            (state.handle)(fork);
            axum::http::StatusCode::OK
        }
        Ok(Decision::Ignored) => axum::http::StatusCode::OK,
        Ok(Decision::BadPayload(_)) => axum::http::StatusCode::BAD_REQUEST,
        Ok(Decision::BadSignature) => axum::http::StatusCode::UNAUTHORIZED,
        Err(_) => axum::http::StatusCode::BAD_REQUEST,
    }
}

/// Build the axum router for the webhook server.
///
/// - `GET /healthz` — liveness probe (always 200 OK).
/// - `POST /api/webhook` — GitHub-configured webhook endpoint.
pub fn router<F>(state: AppState<F>) -> axum::Router
where
    F: Fn(String) + Send + Sync + Clone + 'static,
{
    async fn healthz() -> &'static str {
        "ok"
    }

    axum::Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/api/webhook", axum::routing::post(handler::<F>))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed webhook secret and body with a known-good signature.
    const SECRET: &str = "hunter2-secret";
    const BODY: &[u8] = br#"{"repository":{"full_name":"myorg/terraform-provider-github"}}"#;

    fn good_signature() -> String {
        format!("{SIGNATURE_PREFIX}{}", hmac_hex(SECRET, BODY))
    }

    #[test]
    fn verifies_valid_signature() {
        assert!(verify_signature(SECRET, Some(&good_signature()), BODY));
    }

    #[test]
    fn rejects_missing_or_malformed_signature() {
        assert!(!verify_signature(SECRET, None, BODY));
        assert!(!verify_signature(SECRET, Some(""), BODY));
        // Wrong prefix.
        assert!(!verify_signature(
            SECRET,
            Some(&format!("md5={}", &good_signature()[7..])),
            BODY
        ));
        // Wrong length.
        assert!(!verify_signature(SECRET, Some("sha256=abcd"), BODY));
    }

    #[test]
    fn rejects_tampered_body_or_wrong_secret() {
        // Same signature, different body => no match.
        let other_body = b"{\"repository\":{\"full_name\":\"other/repo\"}}";
        assert!(!verify_signature(
            SECRET,
            Some(&good_signature()),
            other_body
        ));
        // Same body, wrong secret.
        let sig = {
            let mut mac = HmacSha256::new_from_slice(b"wrong-secret").unwrap();
            mac.update(BODY);
            format!(
                "{SIGNATURE_PREFIX}{}",
                hex::encode(mac.finalize().into_bytes())
            )
        };
        assert!(!verify_signature(SECRET, Some(&sig), BODY));
    }

    #[test]
    fn signature_is_content_addressed_not_value_addressed() {
        // A signature computed over a DIFFERENT body must not match this body,
        // even though the hex string is a valid HMAC format.
        let tampered = b"{\"repository\":{\"full_name\":\"evil/repo\"}}";
        let sig = format!("{SIGNATURE_PREFIX}{}", hmac_hex(SECRET, tampered));
        assert!(!verify_signature(SECRET, Some(&sig), BODY));
    }

    #[test]
    fn event_name_maps_known_events() {
        assert_eq!(EventName::from_name("push"), Some(EventName::Push));
        assert_eq!(
            EventName::from_name("pull_request"),
            Some(EventName::PullRequest)
        );
        assert_eq!(EventName::from_name("issues"), None);
        assert_eq!(EventName::from_name(""), None);
    }

    #[test]
    fn dispatch_rejects_bad_signature() {
        let d = dispatch(SECRET, Some("push"), Some("sha256=deadbeef"), BODY).unwrap();
        assert_eq!(d, Decision::BadSignature);
    }

    #[test]
    fn dispatch_ignores_uninteresting_events() {
        let d = dispatch(SECRET, Some("issues"), Some(&good_signature()), BODY).unwrap();
        assert_eq!(d, Decision::Ignored);
    }

    #[test]
    fn dispatch_resolves_fork_for_acting_event() {
        let d = dispatch(SECRET, Some("push"), Some(&good_signature()), BODY).unwrap();
        assert_eq!(
            d,
            Decision::Reconcile {
                fork: "myorg/terraform-provider-github".into()
            }
        );
    }

    #[test]
    fn dispatch_rejects_bad_payload_after_valid_signature() {
        // Valid signature, but non-JSON body: dispatch must error (not panic
        // and not pretend to succeed).
        let body = b"this is not json";
        let sig = format!("{SIGNATURE_PREFIX}{}", hmac_hex(SECRET, body));
        assert!(
            dispatch(SECRET, Some("push"), Some(&sig), body).is_err(),
            "malformed payload should error"
        );
    }

    // ---- handler-level tests (no server / network) ----

    fn run_handler<F: Fn(String) + Send + Sync + Clone + 'static>(
        state: AppState<F>,
        event: Option<&str>,
        sig: Option<&str>,
        body: &[u8],
    ) -> axum::http::StatusCode {
        // Drive the handler synchronously via its core dispatch to exercise the
        // decision->status mapping without spinning up a listener.
        let mut mac = HmacSha256::new_from_slice(state.secret.as_bytes()).unwrap();
        mac.update(body);
        let default_sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let sig = sig.unwrap_or(&default_sig);

        // Build the handler as an async task and block on it.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use axum::http::header::HeaderName;
            let mut headers = HeaderMap::new();
            if let Some(e) = event {
                headers.insert(HeaderName::from_static(EVENT_HEADER), e.parse().unwrap());
            }
            if !sig.is_empty() {
                headers.insert(
                    HeaderName::from_static(SIGNATURE_HEADER),
                    sig.parse().unwrap(),
                );
            }
            handler(State(state), headers, Bytes::copy_from_slice(body)).await
        })
    }

    #[test]
    fn handler_returns_401_for_bad_signature() {
        let state = AppState {
            secret: SECRET.into(),
            handle: |_: String| panic!("must not be called"),
        };
        let status = run_handler(state, Some("push"), Some("sha256=badbadbad"), BODY);
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn handler_returns_200_and_dispatches_for_valid_push() {
        use std::sync::{Arc, Mutex};
        let called: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let state = AppState {
            secret: SECRET.into(),
            handle: {
                let called = called.clone();
                move |fork: String| called.lock().unwrap().push(fork)
            },
        };
        let status = run_handler(state, Some("push"), None, BODY);
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            *called.lock().unwrap(),
            vec!["myorg/terraform-provider-github".to_string()]
        );
    }

    #[test]
    fn handler_returns_200_without_dispatches_for_uninteresting_event() {
        use std::sync::{Arc, Mutex};
        let called: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let state = AppState {
            secret: SECRET.into(),
            handle: {
                let called = called.clone();
                move |_: String| *called.lock().unwrap() = true
            },
        };
        let status = run_handler(state, Some("issues"), None, BODY);
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(!*called.lock().unwrap());
    }

    #[test]
    fn healthz_returns_200() {
        // Drive a real listener on an ephemeral port and hit GET /healthz with
        // a raw HTTP request (no extra client dependency needed).
        let called: std::sync::Arc<std::sync::Mutex<bool>> =
            std::sync::Arc::new(std::sync::Mutex::new(false));
        let state = AppState {
            secret: SECRET.into(),
            handle: {
                let called = called.clone();
                move |_: String| *called.lock().unwrap() = true
            },
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = local.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(local, router(state)).await.unwrap();
            });
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream
                .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            server.abort();
            let status_line = String::from_utf8_lossy(&buf)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            assert!(
                status_line.contains("200 OK"),
                "unexpected response: {status_line}"
            );
            // Return value is not needed; the assert already validated status.
            axum::http::StatusCode::OK
        });
        assert_eq!(status, axum::http::StatusCode::OK);
        let _ = called;
    }
}
