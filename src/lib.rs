//! fork-maintainer — a GitHub App that maintains a fork on top of upstream.
//!
//! The fork's default branch name is the *knob*: it selects which branch of
//! the upstream repository to track. The fork's default branch *contents* are
//! the *artifact*: upstream base tree + applied patch stack + fork-owned files.
//!
//! Branch roles (see `engine`):
//! - `upstream/<X>` — pure mirror of upstream's branch `X`, fast-forward only.
//!   This is the stack trunk that patch PRs target.
//! - `<X>` (fork default) — the recomposed artifact hosting fork workflows and
//!   release tags.
//!
//! The app is event-driven via GitHub webhooks, plus a poll loop for upstream
//! drift (the app cannot subscribe to upstream, only to forks it is installed
//! on).

pub mod config;
pub mod engine;
pub mod github;
