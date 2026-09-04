//! The synthesis engine — base plus ordered patches into one output branch.
//!
//! Everything operates on an ephemeral local bare repository and is a pure
//! function of repository state, which makes the engine unit-testable against
//! local bare repositories with no network.

pub mod fetch;
pub mod pipeline;
pub mod push;
pub mod rebase;
pub mod replay;
pub mod stack;
