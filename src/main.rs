//! synthesize CLI — build a declarative branch from base + ordered patches.
//!
//! ```bash
//! synthesize --base integrations/repo@main \
//!   --patch myorg/repo@fork-owned \
//!   --patch other/repo@feature-x \
//!   --output myorg/repo@main \
//!   --strategy merge
//! ```
//!
//! Alternatively `--config synthesis.json` with the same shape (CLI flags
//! override the file when both are given). The token resolves as
//! `--token`, then `SYNTH_TOKEN`, then `GITHUB_TOKEN`.
//!
//! Exit 0 on success — including the quiet no-op when the output already
//! carries the composed tree. Any failure (fetch, conflict, push) exits
//! non-zero with the error on stderr; on conflict nothing is pushed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use fork_maintainer::config::{BranchRef, Strategy, SynthesisConfig};
use fork_maintainer::engine::pipeline::synthesize_with_urls;
use gix::actor::SignatureRef;

/// Build a declarative branch from a base plus ordered patches.
#[derive(Debug, Parser)]
#[command(name = "synthesize", version)]
struct Args {
    /// Path to a JSON synthesis config file (alternative to flags).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Base branch as `owner/name@branch`.
    #[arg(long)]
    base: Option<String>,
    /// Patch branch as `owner/name@branch`. Repeatable, applied in order.
    /// Replaces config-file patches when at least one is given.
    #[arg(long = "patch")]
    patches: Vec<String>,
    /// Output branch as `owner/name@branch`.
    #[arg(long)]
    output: Option<String>,
    /// Composition strategy: `overlay`, `merge`, or `replay`. Overrides the
    /// config file when given; defaults to the file's value, else `merge`.
    #[arg(long)]
    strategy: Option<String>,
    /// Token for HTTPS git access. Falls back to `SYNTH_TOKEN`, then
    /// `GITHUB_TOKEN`.
    #[arg(long)]
    token: Option<String>,
    /// Scratch directory for the ephemeral bare repo. Defaults to a fresh
    /// directory under the system temp dir.
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// Path to the patch lockfile. Defaults to `synthesis.lock` next to
    /// `--config`, else `./synthesis.lock`. Patches are pinned: a missing
    /// lockfile bootstraps (with a loud warning), drift fails the run.
    #[arg(long)]
    lockfile: Option<PathBuf>,
    /// Re-resolve every patch tip and rewrite the lockfile instead of
    /// enforcing it. Commit the result for review.
    #[arg(long)]
    update_lock: bool,
}

/// Committer identity stamped on synthesized commits; the timestamp is the
/// current time, formatted without external date crates.
const COMMITTER_PREFIX: &str = "fork-maintainer <fork-maintainer@users.noreply.github.com> ";

fn committer() -> Result<SignatureRef<'static>> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before epoch")?
        .as_secs();
    let rendered = format!("{COMMITTER_PREFIX}{secs} +0000");
    // SignatureRef borrows; the process is ephemeral so one intentional leak
    // per run is bounded and invisible.
    let leaked: &'static str = Box::leak(rendered.into_boxed_str());
    SignatureRef::from_bytes(leaked.as_bytes()).context("build committer signature")
}

/// Resolve the token: `--token`, then `SYNTH_TOKEN`, then `GITHUB_TOKEN`.
fn resolve_token(flag: Option<String>) -> Option<String> {
    flag.filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("SYNTH_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Embed a token into an HTTPS URL for git transport; non-HTTPS URLs
/// (`file://`, local paths) pass through untouched.
fn authed_url(url: &str, token: Option<&str>) -> String {
    match token {
        Some(t) if url.starts_with("https://") => {
            url.replacen("https://", &format!("https://x-access-token:{t}@"), 1)
        }
        _ => url.to_string(),
    }
}

fn load_config(args: &Args) -> Result<SynthesisConfig> {
    let mut cfg = match &args.config {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config file {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parse config {}", path.display()))?
        }
        None => SynthesisConfig {
            base: BranchRef::parse_compact(
                args.base
                    .as_deref()
                    .context("--base is required without --config")?,
            )?,
            patches: vec![],
            output: BranchRef::parse_compact(
                args.output
                    .as_deref()
                    .context("--output is required without --config")?,
            )?,
            strategy: Strategy::Merge,
        },
    };
    // Flags override the file when given. (`strategy` always parses: the flag
    // default matches the config default, so an unset flag is a no-op.)
    if let Some(base) = &args.base {
        cfg.base = BranchRef::parse_compact(base)?;
    }
    if !args.patches.is_empty() {
        cfg.patches = args
            .patches
            .iter()
            .map(|p| BranchRef::parse_compact(p))
            .collect::<Result<Vec<_>>>()?;
    }
    if let Some(output) = &args.output {
        cfg.output = BranchRef::parse_compact(output)?;
    }
    if let Some(strategy) = &args.strategy {
        cfg.strategy = Strategy::parse(strategy)?;
    }
    Ok(cfg)
}

fn scratch_dir(workdir: Option<PathBuf>) -> Result<PathBuf> {
    let dir = match workdir {
        Some(dir) => dir,
        None => std::env::temp_dir().join(format!(
            "synthesize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("system clock")?
                .as_nanos()
        )),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create scratch dir {}", dir.display()))?;
    Ok(dir)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let args = Args::parse();

    let cfg = load_config(&args)?;
    let token = resolve_token(args.token);
    tracing::info!(
        base = %cfg.base,
        patches = cfg.patches.len(),
        output = %cfg.output,
        strategy = ?cfg.strategy,
        "synthesizing"
    );

    let dir = scratch_dir(args.workdir)?;
    let repo =
        gix::init_bare(&dir).with_context(|| format!("init bare repo at {}", dir.display()))?;
    let with_auth = |url: &str| authed_url(url, token.as_deref());
    let lock = fork_maintainer::lockfile::LockOptions {
        path: args.lockfile.clone().or_else(|| {
            Some(fork_maintainer::lockfile::default_path(
                args.config.as_deref(),
            ))
        }),
        update: args.update_lock,
    };

    let out = synthesize_with_urls(
        &repo,
        &with_auth(&cfg.base.repo.https_url()),
        &cfg.base.branch,
        &cfg.patches
            .iter()
            .map(|p| (p.clone(), with_auth(&p.repo.https_url())))
            .collect::<Vec<_>>(),
        &with_auth(&cfg.output.repo.https_url()),
        &cfg.output.branch,
        cfg.strategy,
        &lock,
        committer()?,
    )?;

    if out.pushed {
        println!(
            "pushed {} ({} patch{}) to {}",
            out.commit,
            out.patches_applied,
            if out.patches_applied == 1 { "" } else { "es" },
            cfg.output
        );
    } else {
        println!("no change: {} already carries {}", cfg.output, out.tree);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn args() -> Args {
        Args {
            config: None,
            base: None,
            patches: vec![],
            output: None,
            strategy: None,
            token: None,
            workdir: None,
            lockfile: None,
            update_lock: false,
        }
    }

    #[test]
    fn authed_url_embeds_token_only_for_https() {
        assert_eq!(
            authed_url("https://github.com/o/r.git", Some("tok")),
            "https://x-access-token:tok@github.com/o/r.git"
        );
        assert_eq!(
            authed_url("https://github.com/o/r.git", None),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            authed_url("file:///tmp/r.git", Some("tok")),
            "file:///tmp/r.git"
        );
        assert_eq!(authed_url("/tmp/r.git", Some("tok")), "/tmp/r.git");
    }

    #[test]
    fn resolve_token_prefers_flag_then_synth_then_github() {
        // Save and restore: these names are process-global.
        let saved = (
            std::env::var("SYNTH_TOKEN").ok(),
            std::env::var("GITHUB_TOKEN").ok(),
        );
        unsafe {
            std::env::remove_var("SYNTH_TOKEN");
            std::env::remove_var("GITHUB_TOKEN");
        }
        assert_eq!(resolve_token(None), None);
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "g");
        }
        assert_eq!(resolve_token(None).as_deref(), Some("g"));
        unsafe {
            std::env::set_var("SYNTH_TOKEN", "s");
        }
        assert_eq!(resolve_token(None).as_deref(), Some("s"));
        assert_eq!(resolve_token(Some("f".to_string())).as_deref(), Some("f"));
        unsafe {
            std::env::remove_var("SYNTH_TOKEN");
            std::env::remove_var("GITHUB_TOKEN");
            if let Some(v) = &saved.0 {
                std::env::set_var("SYNTH_TOKEN", v);
            }
            if let Some(v) = &saved.1 {
                std::env::set_var("GITHUB_TOKEN", v);
            }
        }
    }

    #[test]
    fn load_config_requires_base_and_output_without_file() {
        let err = load_config(&args()).expect_err("needs base+output");
        assert!(err.to_string().contains("--base"), "got: {err}");
    }

    #[test]
    fn load_config_flags_assemble_full_spec() {
        let mut a = args();
        a.base = Some("up/repo@main".to_string());
        a.patches = vec![
            "me/repo@fork-owned".to_string(),
            "you/repo@feat".to_string(),
        ];
        a.output = Some("me/repo@main".to_string());
        a.strategy = Some("overlay".to_string());
        let cfg = load_config(&a).expect("flags");
        assert_eq!(cfg.base.compact(), "up/repo@main");
        assert_eq!(cfg.patches.len(), 2);
        assert_eq!(cfg.output.compact(), "me/repo@main");
        assert_eq!(cfg.strategy, Strategy::Overlay);
    }

    #[test]
    fn load_config_file_with_flag_overrides() {
        let dir = std::env::temp_dir().join(format!("synth-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("synthesis.json");
        let mut f = std::fs::File::create(&path).expect("create");
        write!(
            f,
            "{{\
                \"base\": {{\"repo\": {{\"owner\": \"u\", \"name\": \"r\"}}, \"branch\": \"main\"}}, \
                \"patches\": [{{\"repo\": {{\"owner\": \"f\", \"name\": \"r\"}}, \"branch\": \"old\"}}], \
                \"output\": {{\"repo\": {{\"owner\": \"f\", \"name\": \"r\"}}, \"branch\": \"main\"}}, \
                \"strategy\": \"overlay\" \
            }}"
        )
        .expect("write");
        let mut a = args();
        a.config = Some(path);
        a.patches = vec!["f/r@new".to_string()];
        let cfg = load_config(&a).expect("file+flags");
        // Flags replace patches; file's base/output/strategy survive.
        assert_eq!(cfg.patches.len(), 1);
        assert_eq!(cfg.patches[0].compact(), "f/r@new");
        assert_eq!(cfg.strategy, Strategy::Overlay);
    }
}
