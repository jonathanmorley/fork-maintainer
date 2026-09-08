//! Agent-assisted conflict resolution — opt-in escape hatch.
//!
//! The default stays fail-closed: unresolved conflicts abort the run for a
//! human. With `--resolve-with <program>` (plus optional `--resolve-model`),
//! an unresolved layer is instead materialized into a scratch directory as
//! `ancestor/`, `ours/`, `theirs/` file sets plus a `TASK.md`, and the agent
//! is invoked non-interactively to write merged results into `resolved/`.
//!
//! The scratch directory lives under the caller's working directory
//! (`.synthesize-resolve-<pid>-<nanos>`, removed on success): agents gate
//! off-project paths behind an external-directory permission that
//! auto-rejects non-interactively, while in-workspace paths work under
//! default policy. Absolute paths are used throughout regardless, since
//! agent servers may resolve relative paths against their own directory.
//!
//! Rules that keep this honest:
//! - Binary (non-UTF-8) paths fail without invoking the agent.
//! - A missing `resolved/` file for any conflicted path fails the run.
//! - Agent failure (nonzero exit) fails the run; nothing is committed.
//! - The agent transcript is logged (audit trail) and the commit records
//!   `Resolved-by:` — resolutions are attributed, never silent.
//! - No verification beyond that: the tool cannot prove a merge correct.
//!   Review the pushed result. Fail-closed remains the default for a reason.

use anyhow::{Context, Result};

/// How to invoke an agent for conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveConfig {
    /// Agent program to execute (e.g. `opencode`, `opencode2` for v2).
    /// Taken literally — no probing, no fallback binaries. Executing the
    /// wrong program here would be a surprise; fail closed instead.
    pub program: String,
    /// Optional model id, passed as `--model <id>` when set.
    pub model: Option<String>,
}

/// One conflicted path with its three sides read from the object store.
/// A side is `None` when that merge stage has no entry for the path.
struct Materialized {
    path: String,
    ancestor: Option<Vec<u8>>,
    ours: Option<Vec<u8>>,
    theirs: Option<Vec<u8>>,
}

/// Judge-then-maybe-resolve for a merge outcome.
///
/// Takes agreed identical additions, then: no unresolved entries →
/// `Ok(false)`; unresolved entries without a resolver → the standard
/// conflict error; with a resolver → agent path, returning `Ok(true)`.
///
/// `repo` backs blob reads and the result tree writes.
pub(crate) fn settle_or_resolve(
    outcome: &mut gix::merge::tree::Outcome<'_>,
    layer: &str,
    repo: &gix::Repository,
    resolve: Option<&ResolveConfig>,
) -> Result<bool> {
    use crate::engine::rebase::{conflict_error, take_agreed_additions, unresolved_paths};

    take_agreed_additions(outcome)?;
    let paths = unresolved_paths(outcome);
    if paths.is_empty() {
        return Ok(false);
    }
    let Some(cfg) = resolve else {
        return Err(conflict_error(layer, &paths));
    };
    resolve_with_agent(repo, outcome, layer, &paths, cfg)?;
    Ok(true)
}

/// Run the agent over the unresolved `paths` in `outcome`, upserting each
/// resolved file into the outcome tree and dropping handled entries.
///
/// Paths must all be currently unresolved; anything else is a caller bug.
///
/// # Errors
///
/// Returns an error when any path is unresolvable by an agent (binary
/// content), the agent fails, or a resolved file is missing.
pub fn resolve_with_agent(
    repo: &gix::Repository,
    outcome: &mut gix::merge::tree::Outcome<'_>,
    layer: &str,
    paths: &[String],
    cfg: &ResolveConfig,
) -> Result<()> {
    use std::fmt::Write as _;

    // Collect first: never invoke the agent when any path is out of scope.
    let mut materialized = Vec::new();
    let mut unresolvable = Vec::new();
    for conflict in &outcome.conflicts {
        use gix::merge::tree::TreatAsUnresolved;
        if !conflict.is_unresolved(TreatAsUnresolved::git()) {
            continue;
        }
        let path = conflict.ours.location().to_string();
        if !paths.contains(&path) {
            continue;
        }
        let entries = conflict.entries();
        // (base, ours, theirs) stage entries, normalized for swapped sides.
        let side = |entry: Option<
            gix::merge::plumbing::tree::ConflictIndexEntry,
        >| -> Result<Option<Vec<u8>>> {
                let Some(entry) = entry else {
                    return Ok(None);
                };
                if entry.mode.kind() != gix::objs::tree::EntryKind::Blob {
                    anyhow::bail!("non-file entry");
                }
                let blob = repo
                    .find_blob(entry.id)
                    .with_context(|| format!("read blob for `{path}`"))?;
                let bytes = blob.data.clone();
                if String::from_utf8(bytes.clone()).is_err() {
                    anyhow::bail!("non-UTF-8 content");
                }
                Ok(Some(bytes))
            };
        match (side(entries[0]), side(entries[1]), side(entries[2])) {
            (Ok(a), Ok(o), Ok(t)) => materialized.push(Materialized {
                path,
                ancestor: a,
                ours: o,
                theirs: t,
            }),
            _ => unresolvable.push(path),
        }
    }
    if !unresolvable.is_empty() {
        unresolvable.sort();
        unresolvable.dedup();
        anyhow::bail!(
            "agent cannot resolve non-text conflicts in `{layer}` at {} path(s): {}. \
             Resolve manually; nothing was pushed",
            unresolvable.len(),
            unresolvable.join(", ")
        );
    }

    // Materialize the scratch dir. Tree locations never contain `..`
    // (rejected by git), but guard anyway: nothing escapes the scratch dir.
    // Scratch lives under the caller's working directory, not the system
    // temp dir: agents gate off-project paths behind an external-directory
    // permission that auto-rejects non-interactively, while in-workspace
    // paths work under default policy. Unique hidden name; removed on
    // success, kept (and reported) on failure for manual resolution.
    let scratch_root = std::env::current_dir().context("resolve working directory")?;
    let scratch = scratch_root.join(format!(
        ".synthesize-resolve-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    for m in &materialized {
        if m.path.split('/').any(|c| c == "..") {
            anyhow::bail!("refusing to materialize suspicious path `{}`", m.path);
        }
        for (dir, content) in [
            ("ancestor", &m.ancestor),
            ("ours", &m.ours),
            ("theirs", &m.theirs),
        ] {
            if let Some(bytes) = content {
                let path = scratch.join(dir).join(&m.path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create scratch dir {}", parent.display()))?;
                }
                std::fs::write(&path, bytes)
                    .with_context(|| format!("write scratch file {}", path.display()))?;
            }
        }
    }
    let task_path = scratch.join("TASK.md");
    let mut task = format!(
        "# Merge conflicts for `{layer}`\n\nWorking directory for all paths below: `{}`\n\nFor each listed path, merge the `ours/` and `theirs/` versions honoring `ancestor/` as the common base, and write the result to `resolved/<path>`. Write the final merged content directly — never emit conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`); a file containing markers is rejected and fails the run. A side marked absent does not exist on that side (treat a missing ancestor as a file both sides added). Every listed path needs a `resolved/` file or the run fails. Do not touch anything else.\n",
        scratch.display(),
    );
    for m in &materialized {
        let _ = writeln!(
            task,
            "\n## {}\n- ancestor: {}\n- ours: {}\n- theirs: {}",
            m.path,
            presence(&m.ancestor),
            presence(&m.ours),
            presence(&m.theirs),
        );
    }
    std::fs::create_dir_all(scratch.join("resolved"))
        .with_context(|| format!("create scratch dir {}", scratch.join("resolved").display()))?;
    std::fs::write(scratch.join("TASK.md"), &task)
        .with_context(|| format!("write {}", scratch.join("TASK.md").display()))?;
    // Machine-readable path list, one per line, for scripted agents.
    std::fs::write(
        scratch.join("PATHS"),
        materialized
            .iter()
            .map(|m| m.path.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .with_context(|| format!("write {}", scratch.join("PATHS").display()))?;

    // Invoke the agent with absolute paths throughout: agent servers may
    // resolve relative paths against their own working directory instead of
    // the child's, which silently points them elsewhere. Absolute paths are
    // robust to either behavior.
    let mut cmd = std::process::Command::new(&cfg.program);
    cmd.arg("run").current_dir(&scratch);
    if let Some(model) = &cfg.model {
        cmd.arg("--model").arg(model);
    }
    cmd.arg(format!(
        "Follow the merge instructions in {}.",
        task_path.display()
    ));
    tracing::info!(program = %cfg.program, scratch = %scratch.display(), "invoking agent");
    let output = cmd.output().with_context(|| {
        format!(
            "failed to execute agent `{}` (is it installed and on PATH?)",
            cfg.program
        )
    })?;
    tracing::info!(
        stdout = %String::from_utf8_lossy(&output.stdout),
        stderr = %String::from_utf8_lossy(&output.stderr),
        "agent transcript"
    );
    if !output.status.success() {
        anyhow::bail!(
            "agent `{}` failed (scratch kept at {}); resolve manually — nothing was pushed",
            cfg.program,
            scratch.display()
        );
    }

    // Read every resolved file back, rejecting conflict markers (an agent
    // punting markers into the file is not a resolution), then drop the
    // handled entries so the outcome no longer reports them.
    let mut handled = Vec::with_capacity(materialized.len());
    for m in &materialized {
        let path = scratch.join("resolved").join(&m.path);
        let bytes = std::fs::read(&path).with_context(|| {
            format!(
                "agent did not write resolved file {} (scratch kept at {}); resolve manually",
                path.display(),
                scratch.display()
            )
        })?;
        if let Some(marker) = conflict_marker(&bytes) {
            anyhow::bail!(
                "agent emitted conflict markers ({marker}) in {}; resolve manually — nothing was pushed",
                path.display()
            );
        }
        let blob = repo.write_blob(&bytes).context("write resolved blob")?;
        outcome
            .tree
            .upsert(&m.path, gix::objs::tree::EntryKind::Blob, blob.detach())?;
        handled.push(m.path.clone());
    }
    outcome
        .conflicts
        .retain(|c| !handled.contains(&c.ours.location().to_string()));
    // Success: tidy the scratch dir (best effort); failures keep it above.
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(())
}

fn presence(content: &Option<Vec<u8>>) -> &'static str {
    if content.is_some() {
        "present"
    } else {
        "absent"
    }
}

/// Detect git conflict markers in resolved content: a line starting with at
/// least seven `<` (ours), exactly seven `=` (separator), or at least seven
/// `>` (theirs). Returns the offending marker kind for error messages.
fn conflict_marker(content: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(content);
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            return Some("<<<<<<<");
        }
        if line == "=======" {
            return Some("=======");
        }
        if line.starts_with(">>>>>>>") {
            return Some(">>>>>>>");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes resolve tests (they share the process cwd for scratch)
    /// and removes any scratch dirs they leave behind, panic or not.
    static SCRATCH_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ScratchGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchGuard {
        fn take() -> Self {
            let lock = SCRATCH_GUARD.lock().expect("test lock");
            Self { _lock: lock }
        }
    }

    impl Drop for ScratchGuard {
        fn drop(&mut self) {
            let Ok(entries) = std::fs::read_dir(".") else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(".synthesize-resolve-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    const AGENT_SH: &str = r#"#!/bin/sh
# Mock agent: resolves every path in PATHS by copying theirs/ (or ours/
# when theirs is absent) into resolved/. Touches $INVOKED_SENTINEL to
# prove invocation.
set -eu
touch "$INVOKED_SENTINEL"
while IFS= read -r p || [ -n "$p" ]; do
  [ -z "$p" ] && continue
  mkdir -p "resolved/$(dirname "$p")"
  if [ -f "theirs/$p" ]; then cp "theirs/$p" "resolved/$p";
  else cp "ours/$p" "resolved/$p"; fi
done < PATHS
"#;

    /// Install the mock agent script; returns its path plus the sentinel
    /// path to assert invocation. The mock runs with cwd=scratch, where it
    /// discovers PATHS, and copies theirs/ (or ours/) into resolved/.
    fn install_mock(dir: &std::path::Path) -> (String, std::path::PathBuf) {
        let script = dir.join("mock-agent.sh");
        std::fs::write(&script, AGENT_SH).expect("write mock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        let sentinel = dir.join("INVOKED");
        std::fs::write(
            dir.join("mock-wrapper.sh"),
            format!(
                "#!/bin/sh\nINVOKED_SENTINEL={} PATHS=PATHS exec {} \"$@\"\n",
                sentinel.display(),
                script.display()
            ),
        )
        .expect("write wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let w = dir.join("mock-wrapper.sh");
            let mut perms = std::fs::metadata(&w).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&w, perms).expect("chmod");
        }
        (dir.join("mock-wrapper.sh").display().to_string(), sentinel)
    }

    /// Merge base/ours/theirs producing the a.txt conflict; returns the
    /// outcome with entries intact.
    fn conflicting_outcome(repo: &gix::Repository) -> gix::merge::tree::Outcome<'_> {
        use gix::objs::tree::EntryKind;
        let sig = gix::actor::SignatureRef::from_bytes(b"t <t@e.c> 1711398853 +0000").expect("sig");
        let mk = |content: &str, parent: Option<gix::ObjectId>| {
            let blob = repo.write_blob(content).expect("blob");
            let mut ed = repo.edit_tree(repo.empty_tree().id).expect("edit");
            ed.upsert("a.txt", EntryKind::Blob, blob.detach())
                .expect("upsert");
            let t = ed.write().expect("write").detach();
            repo.new_commit_as(sig, sig, "m", t, parent)
                .expect("commit")
                .id
        };
        let base = mk("line1\nline2\nline3\nline4\nline5\nline6\n", None);
        let ours_c = mk("line1\nOURS\nline3\nline4\nline5\nline6\n", Some(base));
        let theirs_c = mk("line1\nTHEIRS\nline3\nline4\nline5\nline6\n", Some(base));
        let tree_of = |c: gix::ObjectId| {
            repo.find_commit(c)
                .expect("commit")
                .tree_id()
                .expect("tree")
                .detach()
        };
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some(b"a".as_ref().into()),
            current: Some(b"b".as_ref().into()),
            other: Some(b"c".as_ref().into()),
        };
        let options = repo
            .tree_merge_options()
            .expect("options")
            .with_rewrites(None);
        let outcome = repo
            .merge_trees(
                tree_of(base),
                tree_of(ours_c),
                tree_of(theirs_c),
                labels,
                options,
            )
            .expect("merge");
        // Sanity: this fixture must actually be unresolved, or the tests
        // below prove nothing.
        assert!(
            outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()),
            "fixture must conflict"
        );
        outcome
    }

    #[test]
    fn agent_resolution_lands_in_tree() {
        let _guard = ScratchGuard::take();
        let dir = std::env::temp_dir().join(format!("resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let repo = gix::init_bare(dir.join("repo")).expect("init bare");
        let (program, sentinel) = install_mock(&dir);
        let cfg = ResolveConfig {
            program,
            model: None,
        };
        let mut outcome = conflicting_outcome(&repo);
        let paths: Vec<String> = outcome
            .conflicts
            .iter()
            .map(|c| c.ours.location().to_string())
            .collect();
        resolve_with_agent(&repo, &mut outcome, "test-layer", &paths, &cfg).expect("resolve");
        assert!(sentinel.exists(), "agent was invoked");
        assert!(
            !outcome
                .conflicts
                .iter()
                .any(|c| *c.ours.location() == *"a.txt"),
            "handled entry dropped"
        );
        let tree_id = outcome.tree.write().expect("write").detach();
        let mut tree = repo.find_tree(tree_id).expect("tree");
        let entry = tree
            .peel_to_entry(["a.txt"])
            .expect("peel")
            .expect("present");
        let blob = repo.find_blob(entry.oid().to_owned()).expect("blob");
        assert_eq!(
            String::from_utf8_lossy(&blob.data).as_ref(),
            "line1\nTHEIRS\nline3\nline4\nline5\nline6\n",
            "mock copies theirs"
        );
    }

    #[test]
    fn agent_failure_fails_closed() {
        let _guard = ScratchGuard::take();
        let dir = std::env::temp_dir().join(format!("resolve-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let repo = gix::init_bare(dir.join("repo")).expect("init bare");
        let cfg = ResolveConfig {
            program: "false".to_string(),
            model: None,
        };
        let mut outcome = conflicting_outcome(&repo);
        let paths: Vec<String> = outcome
            .conflicts
            .iter()
            .map(|c| c.ours.location().to_string())
            .collect();
        let err = resolve_with_agent(&repo, &mut outcome, "test-layer", &paths, &cfg)
            .expect_err("agent failure must fail");
        assert!(err.to_string().contains("false"), "got: {err}");
    }

    #[test]
    fn missing_resolved_file_fails() {
        let _guard = ScratchGuard::take();
        let dir = std::env::temp_dir().join(format!("resolve-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let repo = gix::init_bare(dir.join("repo")).expect("init bare");
        // `true` exits 0 without writing anything: resolved files missing.
        let cfg = ResolveConfig {
            program: "true".to_string(),
            model: None,
        };
        let mut outcome = conflicting_outcome(&repo);
        let paths: Vec<String> = outcome
            .conflicts
            .iter()
            .map(|c| c.ours.location().to_string())
            .collect();
        let err = resolve_with_agent(&repo, &mut outcome, "test-layer", &paths, &cfg)
            .expect_err("missing resolved file must fail");
        assert!(
            err.to_string().contains("did not write resolved file"),
            "got: {err}"
        );
    }

    #[test]
    fn no_conflicts_never_invokes_agent() {
        let _guard = ScratchGuard::take();
        let dir = std::env::temp_dir().join(format!("resolve-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (program, sentinel) = install_mock(&dir);
        // Outcome with zero conflicts: settle_or_resolve short-circuits.
        let repo = gix::init_bare(dir.join("repo")).expect("init bare");
        let empty_tree = repo.empty_tree().id;
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some(b"a".as_ref().into()),
            current: Some(b"b".as_ref().into()),
            other: Some(b"c".as_ref().into()),
        };
        let options = repo
            .tree_merge_options()
            .expect("options")
            .with_rewrites(None);
        let mut outcome = repo
            .merge_trees(empty_tree, empty_tree, empty_tree, labels, options)
            .expect("merge");
        let cfg = ResolveConfig {
            program,
            model: None,
        };
        let ran = settle_or_resolve(&mut outcome, "test-layer", &repo, Some(&cfg)).expect("settle");
        assert!(!ran);
        assert!(!sentinel.exists(), "agent must not run without conflicts");
    }

    #[test]
    fn binary_conflict_never_invokes_agent() {
        let _guard = ScratchGuard::take();
        let dir = std::env::temp_dir().join(format!("resolve-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let repo = gix::init_bare(dir.join("repo")).expect("init bare");
        let (program, sentinel) = install_mock(&dir);
        let sig = gix::actor::SignatureRef::from_bytes(b"t <t@e.c> 1711398853 +0000").expect("sig");
        // Sizable binary blobs: tiny ones can pass as text.
        let blob_of = |seed: u8| -> Vec<u8> { (0..500).map(|i| (i % 251) as u8 ^ seed).collect() };
        let mk = |content: &[u8], parent: Option<gix::ObjectId>| {
            let blob = repo.write_blob(content).expect("blob");
            let mut ed = repo.edit_tree(repo.empty_tree().id).expect("edit");
            ed.upsert("bin.dat", gix::objs::tree::EntryKind::Blob, blob.detach())
                .expect("upsert");
            let t = ed.write().expect("write").detach();
            repo.new_commit_as(sig, sig, "m", t, parent)
                .expect("commit")
                .id
        };
        let base = mk(&blob_of(0), None);
        let ours_c = mk(&blob_of(1), Some(base));
        let theirs_c = mk(&blob_of(2), Some(base));
        let tree_of = |c: gix::ObjectId| {
            repo.find_commit(c)
                .expect("commit")
                .tree_id()
                .expect("tree")
                .detach()
        };
        // Merge two distinct binary tips sharing the base: the content
        // merge cannot reconcile them, so the conflict must fail closed
        // without invoking the agent.
        let base_tree = tree_of(base);
        let labels = gix::merge::blob::builtin_driver::text::Labels {
            ancestor: Some(b"a".as_ref().into()),
            current: Some(b"b".as_ref().into()),
            other: Some(b"c".as_ref().into()),
        };
        let options = repo
            .tree_merge_options()
            .expect("options")
            .with_rewrites(None);
        // Merge two distinct binary tips sharing the base.
        let mut outcome = repo
            .merge_trees(
                base_tree,
                tree_of(ours_c),
                tree_of(theirs_c),
                labels,
                options,
            )
            .expect("merge");
        // Only proceed if this actually conflicts; otherwise the test
        // proves nothing (but must still not invoke on success paths).
        let paths: Vec<String> = outcome
            .conflicts
            .iter()
            .map(|c| c.ours.location().to_string())
            .collect();
        let cfg = ResolveConfig {
            program,
            model: None,
        };
        let result = resolve_with_agent(&repo, &mut outcome, "test-layer", &paths, &cfg);
        // Binary content must fail closed without invoking the agent.
        assert!(
            outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()),
            "fixture must conflict or the test proves nothing"
        );
        let err = result.expect_err("binary must fail closed");
        assert!(
            err.to_string().contains("non-text"),
            "binary must fail as non-text, got: {err}"
        );
        assert!(!sentinel.exists(), "agent must not run for binary");
    }

    #[test]
    fn marker_output_rejected() {
        let dir = std::env::temp_dir().join(format!("resolve-mark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let repo = gix::init_bare(dir.join("repo")).expect("init bare");
        // Mock agent that punts conflict markers into the resolved file.
        let script = dir.join("marker-agent.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nset -eu\nmkdir -p resolved\nprintf 'a\\n<<<<<<< ours\\nA\\n=======\\nB\\n>>>>>>> theirs\\nb\\n' > resolved/a.txt\n",
        )
        .expect("write mock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        let _guard = ScratchGuard::take();
        let cfg = ResolveConfig {
            program: script.display().to_string(),
            model: None,
        };
        let mut outcome = conflicting_outcome(&repo);
        let paths: Vec<String> = outcome
            .conflicts
            .iter()
            .map(|c| c.ours.location().to_string())
            .collect();
        let err = resolve_with_agent(&repo, &mut outcome, "test-layer", &paths, &cfg)
            .expect_err("markers must fail");
        assert!(
            err.to_string().contains("conflict markers"),
            "markers must fail as markers, got: {err}"
        );
    }

    #[test]
    fn marker_detection_unit() {
        assert_eq!(conflict_marker(b"a\n<<<<<<< ours\n"), Some("<<<<<<<"));
        assert_eq!(conflict_marker(b"a\n=======\n"), Some("======="));
        assert_eq!(conflict_marker(b"a\n>>>>>>> theirs\n"), Some(">>>>>>>"));
        assert_eq!(conflict_marker(b"a ========= b\n"), None);
        assert_eq!(conflict_marker(b"plain content\n"), None);
    }
}
