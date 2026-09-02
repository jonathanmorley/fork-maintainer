//! The git engine — the reconcile-toward-desired-state core.
//!
//! The engine operates on a local git working copy of the fork (mirrored).
//! Everything is a pure function of the repository state, which makes the
//! engine unit-testable against local bare repositories without any GitHub
//! dependency.
//!
//! Branch roles:
//! - `upstream/<X>` — mirror of upstream's `X` (fast-forward only), the stack
//!   base that patch PRs target.
//! - `<X>` — the recomposed artifact: upstream base tree + patch stack + fork
//!   owned files. Hosts fork workflows and release tags. *(composition is a
//!   later milestone — this module currently focuses on branch syncing)*

pub mod compose;
pub mod fetch;
pub mod sync;