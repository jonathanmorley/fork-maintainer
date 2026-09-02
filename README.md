# fork-maintainer

A GitHub App that maintains forks on top of upstream, keeping them
in sync and their patch stacks clean.

## Design

The fork's **default branch name** is the *knob*: it selects which upstream
branch to track. The default branch **contents** are the *artifact*:
upstream base tree + applied patch stack + fork-owned files.

Two branches the engine maintains:

| Branch | Role |
|---|---|
| `upstream/<X>` | Pure mirror of upstream's `<X>` — **fast-forward only**. This is the stack trunk that patch PRs target. |
| `<X>` (fork default) | The recomposed *artifact* hosting fork workflows and release tags. |

Everything else is derived from the fork's default branch name and the
upstream repository. An optional `override_upstream_branch` allows
pointing at an upstream branch whose name differs from the fork's default.

## Status

**Milestone 1 — Branch syncing engine** ✅ (mirror half)

- [x] Fork config model (`config::ForkConfig`)
- [x] Fast-forward detection (gix `merge_base` + is_ancestor check)
- [x] Fetch upstream over the git transport (`engine::fetch`, `file://` in tests)
- [x] End-to-end mirror sync (`engine::sync::sync_mirror`: fetch + fast-forward)

**Milestone 2 — Tree composition (artifact half)** ✅ (fork-owned preserve)

- [x] Fork-owned file detection (`engine::compose::fork_owned_files`)
- [x] Artifact recomposition (`engine::compose::compose_artifact`): overlay fork-owned
      files onto a new upstream base
- [x] Local bare-repo tests for detection and preservation

**Milestone 3 — Patch stack overlay** ✅ (tree overlay on the base)

- [x] Path-level change enumeration (`engine::stack::patch_changes`, rewrites disabled)
- [x] Change application (`engine::stack::apply_changes`)
- [x] Stack cascade (`engine::stack::apply_patch_stack`): layer ordered patch
      diffs onto a base and write the stacked artifact
- [x] Local bare-repo tests (add/remove/modify, empty stack, multi-patch cascade)

**Up next**

- [ ] Full artifact pipeline: compose fork-owned overlay **and** patch stack together
- [ ] Stack cascade-rebase (behind `Rebase` trait; gix rebase is "idea" stage)
- [ ] GitHub App wiring (webhooks, installation tokens, open-PR discovery)

## Project layout

```
src/
  lib.rs         — library root
  config.rs      — fork config (upstream, fork, mirror branch derivation)
  github.rs      — GitHub API stub (octocrab — placeholder)
  engine/
    mod.rs       — git engine root
    sync.rs      — fast-forward mirror ref + sync_mirror orchestration
    fetch.rs     — fetch upstream over the git transport (remote/explicit refspec)
    compose.rs   — fork-owned file detection + artifact tree composition
    stack.rs     — patch stack overlay (path changes + cascade onto a base)
```

## Development

```bash
cargo build
cargo test
cargo clippy
```

Tests run against temporary bare repositories in the system temp dir, and
fetches use the local `file://` transport. No network or GitHub tokens
required for the current milestone.
