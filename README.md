# fork-maintainer

A GitHub App that maintains forks on top of upstream, keeping them
in sync and their patch stacks clean.

## Design

The fork's **default branch name** is the *knob*: it selects which upstream
branch to track. The default branch **contents** are the *artifact*:
upstream base tree + an ordered stack of fork branches (the fork's persistent
overlays and its patch PRs).

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

**Milestone 2 — Artifact composition** ✅ (uniform stack overlay)

- [x] Path-level change enumeration (`engine::stack::patch_changes`, rewrites disabled)
- [x] Change application (`engine::stack::apply_changes`)
- [x] Artifact composition (`engine::stack::compose`): reset `<X>` to the upstream
      mirror tip, then layer an ordered stack of fork branches on top —
      the fork's persistent overlays (e.g. `.github/`) are just the bottom
      layer of that stack
- [x] Local bare-repo tests (add/remove/modify, empty stack, full artifact with a
      fork-owned layer + patches, and ad-hoc edits on `<X>` being discarded on
      recompose)

**Design note:** fork-owned files are *not* a separate mechanism — they are
just another stacked branch (conceptually an open PR against `upstream/<X>`
that is never merged upstream). Because `<X>` is rebuilt from the upstream base
every cycle, ad-hoc manual edits made directly on `<X>` are discarded; persistent
fork content must live on a stack branch.

**Up next**

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
    stack.rs     — artifact composition (path changes + branch-stack overlay)
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
