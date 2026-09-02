//! GitHub interaction: app identity, installation clients, and PR-stack
//! discovery.
//!
//! This module bridges the engine (which reasons purely about local git state)
//! to GitHub. Two concepts:
//!
//! - [`auth`]: a GitHub App's identity (app id + private key) and the flow for
//!   obtaining a repository installation's credentials — the client that can
//!   make API calls on a fork's behalf, and the HTTPS token used to clone/fetch
//!   that fork.
//! - [`discovery`]: turn a fork's live open PRs into the ordered stack of local
//!   refs the engine composes.
//!
//! The network-dependent pieces (octocrab clients, installation resolution)
//! are thin wrappers; the logic that can run without a network is unit-tested
//! here (credential parsing, stack ordering).

pub mod auth;
pub mod discovery;

pub use discovery::{PrInfo, StackError, discover_stack};
