//! synthesize — build a declarative branch from a base plus ordered patches.
//!
//! A [`config::SynthesisConfig`] declares a base branch and an ordered set of
//! patch branches (each possibly in a different repository). The
//! [`engine`] fetches them into an ephemeral local repo, composes base +
//! patches in order, and force-pushes the single synthesized output branch.
//!
//! The output branch is rebuilt from the base every run: never commit to it
//! directly. All persistent content lives in the base or a patch branch.

pub mod config;
pub mod engine;
