//! Control-plane overlay — managed files applied blind, last.
//!
//! Overlay branches (typically a single `fork-owned` control branch) carry
//! files that must exist verbatim in the output: caller workflows, config,
//! trust policy. They are *not* merged — merging would expose them to
//! spurious conflicts (rename pairing, identical-add edge cases) for zero
//! benefit, since their content is authoritative by definition.
//!
//! Two rules keep this honest instead of merely convenient:
//!
//! 1. **Collision check first.** Every overlay blob path must be absent from
//!    the base tree and from every patch tip tree, unless holding byte-
//!    identical content. Anything else fails loudly naming the path —
//!    silently clobbering base or patch content is never acceptable.
//! 2. **Own commit, skipped when empty.** Application produces exactly one
//!    `overlay control plane` commit on top of the composed tree — or none
//!    when the tree is already identical (idempotency preserved).

use anyhow::{Context, Result};
use gix::{ObjectId, Repository};
use std::collections::HashMap;

/// Flat path → blob id map for a tree, blobs only.
fn entry_map(
    repo: &Repository,
    tree_id: ObjectId,
) -> Result<HashMap<String, (gix::objs::tree::EntryKind, ObjectId)>> {
    fn walk(
        repo: &Repository,
        tree_id: ObjectId,
        prefix: &str,
        out: &mut HashMap<String, (gix::objs::tree::EntryKind, ObjectId)>,
    ) -> Result<()> {
        let tree = repo.find_tree(tree_id)?;
        for entry in tree.iter() {
            let entry = entry?;
            let name = entry.filename().to_string();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let mode = entry.mode();
            let oid = entry.object_id().to_owned();
            if mode.is_tree() {
                walk(repo, oid, &path, out)?;
            } else {
                out.insert(path, (mode.kind(), oid));
            }
        }
        Ok(())
    }
    let mut out = HashMap::new();
    walk(repo, tree_id, "", &mut out)?;
    Ok(out)
}

/// Check overlay trees against base and patch-tip trees.
///
/// Every overlay blob path must be absent everywhere else, or hold
/// byte-identical content. Returns the merged path→blob map (later overlays
/// win) for application.
///
/// # Errors
///
/// Returns an error naming each colliding path and where it collides.
pub fn check_collisions(
    repo: &Repository,
    base_tree: ObjectId,
    patch_trees: &[ObjectId],
    overlay_trees: &[ObjectId],
) -> Result<HashMap<String, (gix::objs::tree::EntryKind, ObjectId)>> {
    let base = entry_map(repo, base_tree).context("read base tree")?;
    let mut patches: HashMap<String, ((gix::objs::tree::EntryKind, ObjectId), usize)> =
        HashMap::new();
    for (i, tree) in patch_trees.iter().enumerate() {
        for (path, entry) in
            entry_map(repo, *tree).with_context(|| format!("read patch tree {i}"))?
        {
            patches.insert(path, (entry, i));
        }
    }
    let mut merged: HashMap<String, (gix::objs::tree::EntryKind, ObjectId)> = HashMap::new();
    let mut collisions = Vec::new();
    for (i, tree) in overlay_trees.iter().enumerate() {
        for (path, entry) in
            entry_map(repo, *tree).with_context(|| format!("read overlay tree {i}"))?
        {
            if let Some(base_entry) = base.get(&path)
                && base_entry != &entry
            {
                collisions.push(format!("{path} (also in base with different content)"));
                continue;
            }
            if let Some((patch_entry, layer)) = patches.get(&path)
                && patch_entry != &entry
            {
                collisions.push(format!(
                    "{path} (also in patch layer {layer} with different content)"
                ));
                continue;
            }
            merged.insert(path, entry);
        }
    }
    if !collisions.is_empty() {
        collisions.sort();
        anyhow::bail!(
            "control-plane collision ({}): {}. Control files must not overlap base or patch content — rename the control file or move the content into a patch.",
            collisions.len(),
            collisions.join("; ")
        );
    }
    Ok(merged)
}

/// Apply an overlay map onto a tree, returning the new tree id.
pub fn apply(
    repo: &Repository,
    base_tree: ObjectId,
    overlay: &HashMap<String, (gix::objs::tree::EntryKind, ObjectId)>,
) -> Result<ObjectId> {
    let mut editor = repo.edit_tree(base_tree)?;
    // Deterministic order for a stable tree hash.
    let mut paths: Vec<&String> = overlay.keys().collect();
    paths.sort();
    for path in paths {
        let (kind, oid) = overlay[path];
        editor
            .upsert(path.as_str(), kind, oid)
            .with_context(|| format!("overlay path `{path}`"))?;
    }
    Ok(editor.write()?.detach())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("fork-maintainer-test")
            .join(format!("overlay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn commit_with_files(
        repo: &Repository,
        files: &[(&str, &[u8])],
        message: &str,
        parent: Option<ObjectId>,
    ) -> ObjectId {
        let sig = gix::actor::SignatureRef::from_bytes(b"t <t@e.c> 1711398853 +0000").expect("sig");
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("edit tree");
        for (name, content) in files {
            let blob = repo.write_blob(content).expect("write blob");
            editor
                .upsert(*name, EntryKind::Blob, blob.detach())
                .expect("upsert");
        }
        let tree_id = editor.write().expect("write tree").detach();
        repo.new_commit_as(sig, sig, message, tree_id, parent)
            .expect("new commit")
            .id
    }

    fn tree_of(repo: &Repository, commit: ObjectId) -> ObjectId {
        repo.find_commit(commit)
            .expect("commit")
            .tree_id()
            .expect("tree")
            .detach()
    }

    fn blob_content(repo: &Repository, tree_id: ObjectId, path: &str) -> Option<Vec<u8>> {
        let mut tree = repo.find_tree(tree_id).expect("tree");
        tree.peel_to_entry(path.split('/')).expect("peel").map(|e| {
            repo.find_blob(e.oid().to_owned())
                .expect("blob")
                .data
                .clone()
        })
    }

    #[test]
    fn disjoint_overlay_applies_cleanly() {
        let dir = temp_dir("clean");
        let repo = gix::init_bare(&dir).expect("init bare");
        let base = commit_with_files(&repo, &[("a.txt", b"a")], "base", None);
        let over = commit_with_files(&repo, &[("managed.yml", b"m")], "over", None);

        let merged = check_collisions(&repo, tree_of(&repo, base), &[], &[tree_of(&repo, over)])
            .expect("no collision");
        assert_eq!(merged.len(), 1);

        let out = apply(&repo, tree_of(&repo, base), &merged).expect("apply");
        assert_eq!(
            blob_content(&repo, out, "managed.yml").as_deref(),
            Some(b"m".as_slice())
        );
        assert_eq!(
            blob_content(&repo, out, "a.txt").as_deref(),
            Some(b"a".as_slice())
        );
    }

    #[test]
    fn identical_content_is_not_a_collision() {
        let dir = temp_dir("identical");
        let repo = gix::init_bare(&dir).expect("init bare");
        let base = commit_with_files(&repo, &[("shared.txt", b"same")], "base", None);
        let over = commit_with_files(&repo, &[("shared.txt", b"same")], "over", None);

        let merged = check_collisions(&repo, tree_of(&repo, base), &[], &[tree_of(&repo, over)])
            .expect("identical content passes");
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn base_collision_fails_naming_path() {
        let dir = temp_dir("basecollide");
        let repo = gix::init_bare(&dir).expect("init bare");
        let base = commit_with_files(&repo, &[("a.txt", b"base-version")], "base", None);
        let over = commit_with_files(&repo, &[("a.txt", b"control-version")], "over", None);

        let err = check_collisions(&repo, tree_of(&repo, base), &[], &[tree_of(&repo, over)])
            .expect_err("collision must fail");
        assert!(err.to_string().contains("a.txt"), "got: {err}");
        assert!(err.to_string().contains("base"), "got: {err}");
    }

    #[test]
    fn patch_collision_names_layer() {
        let dir = temp_dir("patchcollide");
        let repo = gix::init_bare(&dir).expect("init bare");
        let base = commit_with_files(&repo, &[("a.txt", b"a")], "base", None);
        let patch = commit_with_files(&repo, &[("f.txt", b"patch")], "patch", Some(base));
        let over = commit_with_files(&repo, &[("f.txt", b"control")], "over", None);

        let err = check_collisions(
            &repo,
            tree_of(&repo, base),
            &[tree_of(&repo, patch)],
            &[tree_of(&repo, over)],
        )
        .expect_err("collision must fail");
        assert!(err.to_string().contains("patch layer 0"), "got: {err}");
    }

    #[test]
    fn later_overlay_wins() {
        let dir = temp_dir("order");
        let repo = gix::init_bare(&dir).expect("init bare");
        let base = commit_with_files(&repo, &[("a.txt", b"a")], "base", None);
        let o1 = commit_with_files(&repo, &[("m.txt", b"one")], "o1", None);
        let o2 = commit_with_files(&repo, &[("m.txt", b"two")], "o2", None);

        let merged = check_collisions(
            &repo,
            tree_of(&repo, base),
            &[],
            &[tree_of(&repo, o1), tree_of(&repo, o2)],
        )
        .expect("no collision");
        let out = apply(&repo, tree_of(&repo, base), &merged).expect("apply");
        assert_eq!(
            blob_content(&repo, out, "m.txt").as_deref(),
            Some(b"two".as_slice())
        );
    }
}
