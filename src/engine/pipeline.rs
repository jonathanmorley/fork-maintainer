//! Synthesis pipeline — base plus declared patches into one output branch.
//!
//! A single pass for a [`SynthesisConfig`](crate::config::SynthesisConfig):
//!
//! 1. **Fetch** the base branch and every patch branch — each from its own
//!    repository URL, into an ephemeral local repo. Patches may live in any
//!    repository; composition only ever sees trees.
//! 2. **Compose** the output tree: the base with each patch layered on top in
//!    declared order, via the selected [`Rebase`] strategy.
//! 3. **Push** the output branch — forced, since synthesis rebuilds it every
//!    run. When the composed tree is identical to the current output tip,
//!    the push is skipped (`pushed: false`) so scheduled runs are quiet no-ops.
//!
//! URLs carry authentication when needed (the caller embeds a token for HTTPS
//! remotes); this module is auth-agnostic.
//!
//! # Blocking
//!
//! This pass uses gix's blocking transport (fetch) and blocking object I/O,
//! plus the `git` CLI for push. It is synchronous throughout.

use anyhow::{Context, Result};
use gix::{Repository, actor::SignatureRef};

use crate::config::{BranchRef, Strategy};
use crate::engine::fetch::fetch_upstream;
use crate::engine::push::push_output;
use crate::engine::rebase::{ComposeOutcome, Merge, Overlay, Rebase};

/// Local ref holding the fetched base.
const BASE_REF: &str = "refs/synthesis/base";
/// Local ref holding the freshly composed output before push.
const OUTPUT_REF: &str = "refs/synthesis/output";
/// Local ref holding the previously published output tip, when it exists.
const PREV_OUTPUT_REF: &str = "refs/synthesis/output-prev";

/// The outcome of a synthesis pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizeOutcome {
    /// The composed tree id.
    pub tree: gix::ObjectId,
    /// The synthesized commit id.
    pub commit: gix::ObjectId,
    /// How many patch layers were applied.
    pub patches_applied: usize,
    /// Whether the output branch was pushed. False when the composed tree
    /// already matches the published tip (quiet no-op).
    pub pushed: bool,
}

/// Resolve a [`Strategy`] to its [`Rebase`] implementation.
///
/// # Errors
///
/// This cannot fail today (both strategies are implemented); the `Result`
/// keeps the seam open for future strategies.
fn strategy_impl(strategy: Strategy) -> Result<Box<dyn Rebase>> {
    match strategy {
        Strategy::Overlay => Ok(Box::new(Overlay)),
        Strategy::Merge => Ok(Box::new(Merge)),
    }
}

/// Fetch a branch from `url` into `local_ref`, returning its tip.
///
/// # Errors
///
/// Returns an error when the remote does not advertise the branch.
fn fetch_branch(
    repo: &Repository,
    url: &str,
    branch: &str,
    local_ref: &str,
) -> Result<gix::ObjectId> {
    fetch_upstream(repo, url, branch, local_ref)
        .with_context(|| format!("fetch branch `{branch}` from `{url}`"))
        .map(|tip| tip.oid)
}

/// Try to fetch the current output tip; `Ok(None)` on a first run where the
/// output branch does not exist yet.
///
/// A missing branch surfaces from fetch as an error mentioning the ref; that
/// specific case maps to `None`, everything else propagates.
fn fetch_prev_output(repo: &Repository, url: &str, branch: &str) -> Result<Option<gix::ObjectId>> {
    match fetch_upstream(repo, url, branch, PREV_OUTPUT_REF) {
        Ok(tip) => Ok(Some(tip.oid)),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("did not advertise") || msg.contains(&format!("refs/heads/{branch}")) {
                Ok(None)
            } else {
                Err(e).with_context(|| format!("fetch output branch `{branch}` from `{url}`"))
            }
        }
    }
}

/// Previous output tip's tree, if the ref exists locally.
fn ref_tree(repo: &Repository, r: &str) -> Option<gix::ObjectId> {
    let oid = repo.find_reference(r).ok()?.id().detach();
    repo.find_commit(oid)
        .ok()?
        .tree_id()
        .ok()
        .map(|t| t.detach())
}

/// Run one synthesis pass: fetch, compose, and push-if-changed.
///
/// `repo` is an ephemeral bare repository (fresh per run is fine).
/// `base` selects the base branch, `patches` the ordered layers, `output`
/// the branch to advance. URLs embed authentication when required.
pub fn synthesize(
    repo: &Repository,
    base: &BranchRef,
    patches: &[BranchRef],
    output: &BranchRef,
    strategy: Strategy,
    committer: SignatureRef<'_>,
) -> Result<SynthesizeOutcome> {
    synthesize_with_urls(
        repo,
        &base.repo.https_url(),
        &base.branch,
        &patches
            .iter()
            .map(|p| (p.repo.https_url(), p.branch.clone()))
            .collect::<Vec<_>>(),
        &output.repo.https_url(),
        &output.branch,
        strategy,
        committer,
    )
}

/// URL-level synthesis: the same pass with prebuilt transport URLs.
///
/// Callers embed tokens into HTTPS URLs (see
/// [`crate::config::Repo::authed_https_url`]) before calling; `file://` and
/// plain paths work unchanged for tests and local runs.
#[allow(clippy::too_many_arguments)]
pub fn synthesize_with_urls(
    repo: &Repository,
    base_url: &str,
    base_branch: &str,
    patches: &[(String, String)],
    output_url: &str,
    output_branch: &str,
    strategy: Strategy,
    committer: SignatureRef<'_>,
) -> Result<SynthesizeOutcome> {
    // 1. Fetch the base and every patch branch into local refs.
    fetch_branch(repo, base_url, base_branch, BASE_REF)?;
    let mut patch_refs = Vec::with_capacity(patches.len());
    for (i, (url, branch)) in patches.iter().enumerate() {
        let local = format!("refs/synthesis/patch-{i}");
        fetch_branch(repo, url, branch, &local)?;
        patch_refs.push(local);
    }

    // 2. Compose the output tree from base + patches in order.
    let strategy_impl = strategy_impl(strategy)?;
    let ComposeOutcome {
        tree,
        commit,
        patches_applied,
    } = strategy_impl
        .compose(repo, BASE_REF, &patch_refs, OUTPUT_REF, committer)
        .with_context(|| {
            format!(
                "compose {} patch(es) onto {base_branch} from {base_url}",
                patches.len()
            )
        })?;

    // 3. Skip the push when the published tip already carries this tree.
    if let Some(_prev) = fetch_prev_output(repo, output_url, output_branch)?
        && ref_tree(repo, PREV_OUTPUT_REF).as_ref() == Some(&tree)
    {
        return Ok(SynthesizeOutcome {
            tree,
            commit,
            patches_applied,
            pushed: false,
        });
    }

    // 4. Force-push the synthesized output branch.
    let repo_path = repo.workdir().unwrap_or_else(|| repo.common_dir());
    push_output(repo_path, output_url, OUTPUT_REF, output_branch, None)
        .with_context(|| format!("push output branch `{output_branch}` to `{output_url}`"))?;

    Ok(SynthesizeOutcome {
        tree,
        commit,
        patches_applied,
        pushed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;
    use gix::refs::transaction::PreviousValue;
    use std::path::PathBuf;

    const SIG: &[u8] = b"tester <tester@example.com> 1711398853 +0000";

    fn sig() -> SignatureRef<'static> {
        SignatureRef::from_bytes(SIG).expect("valid sig")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("synth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn commit_with_files(
        repo: &Repository,
        files: &[(&str, &str)],
        message: &str,
        parent: Option<gix::ObjectId>,
    ) -> gix::ObjectId {
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        for (name, content) in files {
            let blob = repo.write_blob(content).expect("write blob");
            editor
                .upsert(*name, EntryKind::Blob, blob.detach())
                .expect("upsert");
        }
        let tree_id = editor.write().expect("write tree").detach();
        repo.new_commit_as(sig(), sig(), message, tree_id, parent)
            .expect("new commit")
            .id
    }

    fn set_ref(repo: &Repository, name: &str, target: gix::ObjectId) {
        repo.reference(name, target, PreviousValue::Any, "set ref for test")
            .expect("set ref");
    }

    fn ref_id(repo: &Repository, name: &str) -> Option<gix::ObjectId> {
        repo.find_reference(name).ok().map(|r| r.id().detach())
    }

    fn tree_blob(repo: &Repository, tree_id: gix::ObjectId, path: &str) -> Option<String> {
        let mut tree = repo.find_tree(tree_id).expect("find tree");
        tree.peel_to_entry(path.split('/'))
            .expect("peel")
            .map(|entry| {
                let blob = repo.find_blob(entry.oid().to_owned()).expect("find blob");
                String::from_utf8_lossy(&blob.data).into_owned()
            })
    }

    fn url(dir: &std::path::Path) -> String {
        dir.display().to_string()
    }

    /// Base + one cross-repo patch compose and push the output branch.
    #[test]
    fn synthesizes_base_plus_cross_repo_patch() {
        // Upstream repo carrying a.txt.
        let upstream_dir = temp_dir("up");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let base = commit_with_files(&upstream, &[("a.txt", "a1")], "base", None);
        set_ref(&upstream, "refs/heads/main", base);

        // A *different* repo carrying the patch branch.
        let patch_dir = temp_dir("patchrepo");
        let patch_repo = gix::init_bare(&patch_dir).expect("init patch repo");
        let patch = commit_with_files(
            &patch_repo,
            &[("a.txt", "a1"), ("feat.txt", "f")],
            "feat",
            None,
        );
        set_ref(&patch_repo, "refs/heads/feature", patch);

        // Output repo starts empty.
        let output_dir = temp_dir("out");
        let _output = gix::init_bare(&output_dir).expect("init output");

        // Scratch repo for the run.
        let scratch_dir = temp_dir("scratch");
        let scratch = gix::init_bare(&scratch_dir).expect("init scratch");

        let out = synthesize_with_urls(
            &scratch,
            &url(&upstream_dir),
            "main",
            &[(url(&patch_dir), "feature".to_string())],
            &url(&output_dir),
            "main",
            Strategy::Merge,
            sig(),
        )
        .expect("synthesize");

        assert!(out.pushed);
        assert_eq!(out.patches_applied, 1);
        assert_eq!(
            tree_blob(&scratch, out.tree, "feat.txt").as_deref(),
            Some("f")
        );

        let output = gix::open(&output_dir).expect("open output");
        let tip = ref_id(&output, "refs/heads/main").expect("output pushed");
        let tip_tree = output
            .find_commit(tip)
            .expect("find tip")
            .tree_id()
            .expect("tip tree")
            .detach();
        assert_eq!(tip_tree, out.tree);
    }

    /// A second identical run is a quiet no-op (no push).
    #[test]
    fn second_identical_run_skips_push() {
        let upstream_dir = temp_dir("up2");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let base = commit_with_files(&upstream, &[("a.txt", "a1")], "base", None);
        set_ref(&upstream, "refs/heads/main", base);

        let output_dir = temp_dir("out2");
        let _output = gix::init_bare(&output_dir).expect("init output");

        let run = |tag: &str| {
            let scratch_dir = temp_dir(&format!("scratch2-{tag}"));
            let scratch = gix::init_bare(&scratch_dir).expect("init scratch");
            synthesize_with_urls(
                &scratch,
                &url(&upstream_dir),
                "main",
                &[],
                &url(&output_dir),
                "main",
                Strategy::Overlay,
                sig(),
            )
            .expect("synthesize")
        };

        let first = run("a");
        assert!(first.pushed, "first run pushes");

        let output = gix::open(&output_dir).expect("open output");
        let tip_before = ref_id(&output, "refs/heads/main").expect("tip");

        let second = run("b");
        assert!(!second.pushed, "second run skips push");
        assert_eq!(second.tree, first.tree);

        let tip_after = ref_id(&output, "refs/heads/main").expect("tip");
        assert_eq!(tip_before, tip_after, "output ref untouched");
    }

    /// A conflicting patch fails the run and pushes nothing.
    #[test]
    fn conflicting_patch_fails_without_pushing() {
        let upstream_dir = temp_dir("up3");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let base = commit_with_files(&upstream, &[("a.txt", "a1")], "base", None);
        set_ref(&upstream, "refs/heads/main", base);

        // Patch A moves a.txt to a2, forked from equivalent base content in
        // its own repo (each patch repo is self-contained; merge is
        // content-based, so the SHAs need not match upstream's).
        let repo_a_dir = temp_dir("repoa");
        let repo_a = gix::init_bare(&repo_a_dir).expect("init a");
        let base_a = commit_with_files(&repo_a, &[("a.txt", "a1")], "base", None);
        let a = commit_with_files(&repo_a, &[("a.txt", "a2")], "a", Some(base_a));
        set_ref(&repo_a, "refs/heads/a", a);

        // Patch B moves a.txt to a3, forked from the same base content.
        let repo_b_dir = temp_dir("repob");
        let repo_b = gix::init_bare(&repo_b_dir).expect("init b");
        let base_b = commit_with_files(&repo_b, &[("a.txt", "a1")], "base", None);
        let b = commit_with_files(&repo_b, &[("a.txt", "a3")], "b", Some(base_b));
        set_ref(&repo_b, "refs/heads/b", b);

        let output_dir = temp_dir("out3");
        let _output = gix::init_bare(&output_dir).expect("init output");

        let scratch_dir = temp_dir("scratch3");
        let scratch = gix::init_bare(&scratch_dir).expect("init scratch");

        let err = synthesize_with_urls(
            &scratch,
            &url(&upstream_dir),
            "main",
            &[
                (url(&repo_a_dir), "a".to_string()),
                (url(&repo_b_dir), "b".to_string()),
            ],
            &url(&output_dir),
            "main",
            Strategy::Merge,
            sig(),
        )
        .expect_err("conflicting patches should fail");

        // anyhow's Display shows only the outer context; `{:#}` renders the
        // full chain where the conflict detail lives.
        let msg = format!("{err:#}");
        assert!(msg.contains("conflicts detected"), "got: {msg}");
        assert!(msg.contains("a.txt"), "got: {msg}");

        let output = gix::open(&output_dir).expect("open output");
        assert_eq!(
            ref_id(&output, "refs/heads/main"),
            None,
            "nothing pushed on conflict"
        );
    }

    /// A missing patch branch fails with the branch named.
    #[test]
    fn missing_patch_branch_errors() {
        let upstream_dir = temp_dir("up4");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let base = commit_with_files(&upstream, &[("a.txt", "a1")], "base", None);
        set_ref(&upstream, "refs/heads/main", base);

        let output_dir = temp_dir("out4");
        let _output = gix::init_bare(&output_dir).expect("init output");

        let scratch_dir = temp_dir("scratch4");
        let scratch = gix::init_bare(&scratch_dir).expect("init scratch");

        let err = synthesize_with_urls(
            &scratch,
            &url(&upstream_dir),
            "main",
            &[(url(&upstream_dir), "nope".to_string())],
            &url(&output_dir),
            "main",
            Strategy::Overlay,
            sig(),
        )
        .expect_err("missing patch should fail");
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    /// Merge strategy applies every commit of a multi-commit patch branch.
    #[test]
    fn merge_applies_multi_commit_patch_fully() {
        let upstream_dir = temp_dir("up5");
        let upstream = gix::init_bare(&upstream_dir).expect("init upstream");
        let base = commit_with_files(&upstream, &[("a.txt", "a1")], "base", None);
        set_ref(&upstream, "refs/heads/main", base);

        // Patch repo: two commits sharing history. The root replicates the
        // upstream base commit byte-for-byte (same files, message, sig), so
        // both object stores unify on one SHA and merge-base links the
        // histories — exactly as fetched real branches do. The first patch
        // commit adds feat.txt, the second modifies a.txt.
        let patch_dir = temp_dir("patch5");
        let patch_repo = gix::init_bare(&patch_dir).expect("init patch repo");
        let c0 = commit_with_files(&patch_repo, &[("a.txt", "a1")], "base", None);
        assert_eq!(c0, base, "replicated root must unify with upstream base");
        let c1 = commit_with_files(
            &patch_repo,
            &[("a.txt", "a1"), ("feat.txt", "f")],
            "add feat",
            Some(c0),
        );
        let c2 = commit_with_files(
            &patch_repo,
            &[("a.txt", "a2"), ("feat.txt", "f")],
            "tweak a",
            Some(c1),
        );
        set_ref(&patch_repo, "refs/heads/feature", c2);

        let output_dir = temp_dir("out5");
        let _output = gix::init_bare(&output_dir).expect("init output");

        let scratch_dir = temp_dir("scratch5");
        let scratch = gix::init_bare(&scratch_dir).expect("init scratch");

        let out = synthesize_with_urls(
            &scratch,
            &url(&upstream_dir),
            "main",
            &[(url(&patch_dir), "feature".to_string())],
            &url(&output_dir),
            "main",
            Strategy::Merge,
            sig(),
        )
        .expect("synthesize");

        assert!(out.pushed);
        // Both the early commit's file and the tip's modification land.
        assert_eq!(
            tree_blob(&scratch, out.tree, "feat.txt").as_deref(),
            Some("f")
        );
        assert_eq!(
            tree_blob(&scratch, out.tree, "a.txt").as_deref(),
            Some("a2")
        );
    }

    /// Stacked boilerplate files across patches merge cleanly.
    ///
    /// Characterization test (not a regression guard): two stacked patches
    /// adding similar boilerplate must compose without conflict. The live
    /// trigger for this failure class needed real-scale content — the proven
    /// guard is `auto_resolved_blob_merge_does_not_fail` in rebase.rs, which
    /// uses blobs extracted from the live run.
    #[test]
    fn merge_ignores_similar_files_across_patches() {
        let boilerplate = |name: &str| {
            (1..=10)
                .map(|i| {
                    if i == 5 {
                        format!("resource {name} line\n")
                    } else {
                        format!("common boilerplate line {i}\n")
                    }
                })
                .collect::<String>()
        };

        // One repo holding base and a two-patch stack, like real branches.
        let remote_dir = temp_dir("sim_remote");
        let remote = gix::init_bare(&remote_dir).expect("init remote");
        let boiler_a = boilerplate("aaa");
        let boiler_b = boilerplate("bbb");
        let base = commit_with_files(&remote, &[("base.txt", "b")], "base", None);
        set_ref(&remote, "refs/heads/main", base);
        let a = commit_with_files(
            &remote,
            &[("base.txt", "b"), ("a/example.txt", &boiler_a)],
            "patch a",
            Some(base),
        );
        set_ref(&remote, "refs/heads/patch-a", a);
        let b = commit_with_files(
            &remote,
            &[
                ("base.txt", "b"),
                ("a/example.txt", &boiler_a),
                ("b/example.txt", &boiler_b),
            ],
            "patch b",
            Some(a),
        );
        set_ref(&remote, "refs/heads/patch-b", b);

        let output_dir = temp_dir("sim_out");
        let _output = gix::init_bare(&output_dir).expect("init output");

        let scratch_dir = temp_dir("sim_scratch");
        let scratch = gix::init_bare(&scratch_dir).expect("init scratch");

        let out = synthesize_with_urls(
            &scratch,
            &url(&remote_dir),
            "main",
            &[
                (url(&remote_dir), "patch-a".to_string()),
                (url(&remote_dir), "patch-b".to_string()),
            ],
            &url(&output_dir),
            "main",
            Strategy::Merge,
            sig(),
        )
        .expect("similar files across patches must merge cleanly");

        assert!(out.pushed);
        assert_eq!(
            tree_blob(&scratch, out.tree, "a/example.txt").as_deref(),
            Some(boiler_a.as_str())
        );
        assert_eq!(
            tree_blob(&scratch, out.tree, "b/example.txt").as_deref(),
            Some(boiler_b.as_str())
        );
    }
}
