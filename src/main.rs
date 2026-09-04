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
use fork_maintainer::config::{BranchRef, PatchSpec, Strategy, SynthesisConfig};
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
    /// Maintenance update: refresh the lockfile and drop merged patches,
    /// writing the caller files for review (never pushes or deletes).
    /// Requires --config. Pair with --dry-run to preview.
    #[arg(long)]
    update: bool,
    /// Preview file writes without executing them (with --update).
    #[arg(long)]
    dry_run: bool,
    /// Scaffold a managed fork: create the control branch locally with
    /// caller workflows + config. Never pushes unless --push is set.
    #[arg(long)]
    init: bool,
    /// Upstream base as `owner/name@branch` (with --init).
    #[arg(long)]
    upstream: Option<String>,
    /// Fork under adoption as `owner/name` (with --init).
    #[arg(long)]
    fork: Option<String>,
    /// Output branch name in the fork (with --init). Defaults to `main`.
    #[arg(long)]
    output_branch: Option<String>,
    /// Control branch to create (with --init). Defaults to `fork-owned`.
    #[arg(long)]
    branch: Option<String>,
    /// Fresh root commit for the control branch instead of forking the
    /// upstream tip (with --init).
    #[arg(long)]
    fresh: bool,
    /// Push the created control branch (with --init). Default prints the
    /// push command instead.
    #[arg(long)]
    push: bool,
    /// Engine ref to pin in generated caller workflows (with --init).
    /// Defaults to resolving fork-maintainer main at runtime.
    #[arg(long)]
    engine_ref: Option<String>,
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
            .map(|p| {
                let branch = BranchRef::parse_compact(p)?;
                Ok::<_, anyhow::Error>(PatchSpec { branch, pin: true })
            })
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

/// Maintenance update: refresh caller files for review.
///
/// Resolves current patch tips, collects PR states, plans file changes, and
/// writes the config + lockfile (or previews with --dry-run). Never pushes
/// output, deletes branches, or closes PRs.
///
/// Requires --config (the managed file pattern). Requires a token for PR
/// state; without one the update cannot judge merges.
fn run_update(args: &Args) -> Result<()> {
    use fork_maintainer::update as maintenance;

    let config_path = args
        .config
        .as_deref()
        .context("--update requires --config (the managed file pattern)")?;
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config file {}", config_path.display()))?;
    let cfg: SynthesisConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse config {}", config_path.display()))?;
    let lock_path = args
        .lockfile
        .clone()
        .unwrap_or_else(|| fork_maintainer::lockfile::default_path(Some(config_path)));
    let token = resolve_token(args.token.clone())
        .context("--update needs a token for PR state (--token, SYNTH_TOKEN, or GITHUB_TOKEN)")?;

    let github =
        maintenance::GitHub::new("fork-maintainer/synthesize-update")?.with_token(token.clone());
    let with_auth = |url: &str| authed_url(url, Some(&token));
    let url_for = |repo: &fork_maintainer::config::Repo| with_auth(&repo.https_url());

    let lock = fork_maintainer::lockfile::load(&lock_path)?;
    let statuses = maintenance::observe_patches(&github, &cfg.patches, &cfg.output, &url_for)?;
    let plan = maintenance::plan_updates(&statuses, lock.as_ref())?;

    if plan.is_empty() {
        println!("update: inputs current, nothing to propose");
        return Ok(());
    }
    maintenance::apply_to_files(
        config_path,
        &lock_path,
        &cfg,
        &statuses,
        &plan,
        args.dry_run,
    )?;
    if args.dry_run {
        println!("update: dry run, files unchanged");
    } else {
        println!(
            "update: proposed {} removal{} and lock refresh; review the diff",
            plan.removed.len(),
            if plan.removed.len() == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

/// Scaffold a managed fork (see [`fork_maintainer::init`]).
///
/// Resolves the engine pin, discovers open PRs, and delegates filesystem
/// work to [`fork_maintainer::init::scaffold`]. Prints the human cutover
/// sequence afterwards.
fn run_init(args: &Args) -> Result<()> {
    use fork_maintainer::init as scaffolding;

    let upstream = BranchRef::parse_compact(
        args.upstream
            .as_deref()
            .context("--upstream is required with --init")?,
    )?;
    let fork = args
        .fork
        .as_deref()
        .context("--fork is required with --init (owner/name)")?;
    let (owner, name) = fork.split_once('/').context("--fork must be owner/name")?;
    if owner.is_empty() || name.is_empty() {
        anyhow::bail!("--fork must be owner/name, got `{fork}`");
    }
    let fork = fork_maintainer::config::Repo {
        owner: owner.to_string(),
        name: name.to_string(),
    };
    let workdir = args
        .workdir
        .clone()
        .unwrap_or_else(scaffolding::default_workdir);
    if !workdir.join(".git").exists() {
        anyhow::bail!(
            "--workdir {} is not a git checkout of the fork",
            workdir.display()
        );
    }

    let token = resolve_token(args.token.clone());
    let with_auth = |url: &str| authed_url(url, token.as_deref());
    let url_for = |repo: &fork_maintainer::config::Repo| with_auth(&repo.https_url());

    // Engine pin: explicit flag, else live upstream tip, else branch name.
    let engine_pin = match &args.engine_ref {
        Some(r) => r.clone(),
        None => match fork_maintainer::update::resolve_tip(
            "https://github.com/jonathanmorley/fork-maintainer.git",
            "main",
        ) {
            Ok(Some(sha)) => sha.to_string(),
            _ => {
                tracing::warn!("could not resolve engine pin; using branch name");
                "main".to_string()
            }
        },
    };

    let github = scaffolding_github(token.clone())?;
    let open = github.open_prs(&fork)?;
    let opts = scaffolding::InitOptions {
        base: upstream,
        fork: fork.clone(),
        output_branch: args
            .output_branch
            .clone()
            .unwrap_or_else(|| "main".to_string()),
        control_branch: args
            .branch
            .clone()
            .unwrap_or_else(|| "fork-owned".to_string()),
        fresh: args.fresh,
        engine_ref: engine_pin,
    };
    let report = scaffolding::scaffold(&workdir, &opts, &open, &url_for, args.push)?;
    println!(
        "init: created branch {} with {} files ({} patch proposals{})",
        report.branch,
        report.files.len(),
        report.patches.len(),
        if args.push { ", pushed" } else { "" },
    );
    println!("{}", report.next_steps(&fork));
    Ok(())
}

fn scaffolding_github(token: Option<String>) -> Result<fork_maintainer::update::GitHub> {
    let mut github = fork_maintainer::update::GitHub::new("fork-maintainer/synthesize-init")?;
    if let Some(t) = token {
        github = github.with_token(t);
    }
    Ok(github)
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

    if args.update {
        return run_update(&args);
    }

    if args.init {
        return run_init(&args);
    }

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
            .map(|p| (p.clone(), with_auth(&p.branch.repo.https_url())))
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
            update: false,
            dry_run: false,
            init: false,
            upstream: None,
            fork: None,
            output_branch: None,
            branch: None,
            fresh: false,
            push: false,
            engine_ref: None,
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
        assert!(cfg.patches.iter().all(|p| p.pin), "flags pin by default");
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
        assert_eq!(cfg.patches[0].branch.compact(), "f/r@new");
        assert_eq!(cfg.strategy, Strategy::Overlay);
    }
}
