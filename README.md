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

**Milestone 3 — Reconcile pipeline** ✅ (config + sync + compose in one pass)

- [x] `engine::pipeline::reconcile`: sync the `upstream/<X>` mirror (fetch +
      fast-forward), then compose the artifact on top of the freshly synced tip
- [x] Stack branch changes are diffed against each branch's **own fork point**
      (first-parent), so files upstream adds after a branch forks are not
      misread as branch deletions
- [x] Derived refs from `ForkConfig` (mirror, tracking, artifact; respects
      `override_upstream_branch`); end-to-end tests wired to config

**Milestone 4 — Open-PR stack discovery** ✅ (ordering seam, no network)

- [x] `github::discover_stack`: turn a fork's open PRs (head/base) into the
      ordered stack refs, fork-owned branch first, then PRs topological by
      base chain (same-level take ascending number); deterministic
- [x] Errors on unknown bases and PR cycles
- [x] End-to-end `discover_stack` -> `reconcile` test over a local repo

**Design note:** fork-owned files are *not* a separate mechanism — they are
just another stacked branch (conceptually an open PR against `upstream/<X>`
that is never merged upstream). Because `<X>` is rebuilt from the upstream base
every cycle, ad-hoc manual edits made directly on `<X>` are discarded; persistent
fork content must live on a stack branch.

**Milestone 5 — GitHub App identity** ✅ (credentials + installation clients)

- [x] `github::auth::AppCredentials`: app id + RSA private key, with PEM
      parsing validated at build time (`encoding_key()` errors on invalid PEM)
- [x] `app_client()`: build the app-authenticated `octocrab` client
- [x] `install_client()`: resolve a repo's installation and return a scoped
      client; `install_https_token()` for HTTPS git access
- [x] Unit tests: valid key parses, invalid key fails, client builds with a
      valid key (via `#[tokio::test]`)

**Milestone 6 — live open-PR discovery + PR-head fetch** ✅

- [x] `github::live_prs()`: page a fork's open PRs via octocrab and reduce them
      to `PrInfo` (number, head branch, base branch)
- [x] `PrInfo::from_pull_request()`: pure octocrab-model → `PrInfo` mapping
- [x] `engine::fetch::fetch_pr_head()`: bring `refs/pull/<n>/head` into the
      local mirror from the fork
- [x] `engine::fetch::fetch_pull_refs()`: for an ordered stack, fetch every
      `refs/pull/<n>/head` member (skips non-pull refs) — the bridge from
      `discover_stack` output to `reconcile`
- [x] `engine::fetch::fetch_ref()`: shared low-level single-ref fetch helper

**Milestone 7 — webhook server** ✅ (signature verification + event dispatch)

- [x] `webhook::verify_signature()`: constant-time HMAC-SHA256 check of
      `X-Hub-Signature-256` against the raw body keyed by the webhook secret
- [x] `webhook::EventName`: classify `push` / `pull_request` vs. ignored events
- [x] `webhook::dispatch()`: pure decision (bad signature / ignored / bad
      payload / reconcile fork) from the request triple
- [x] `webhook::handler()` + `router()`: axum `POST /api/webhook` endpoint,
      mapping decisions to HTTP status (401/400/200), invoking an injected
      per-fork action
- [x] `main.rs`: boot the server from `AppConfig` (`FORK_MAINTAINER_CONFIG` /
      `config.json`), resolve the affected fork and request a reconcile
- [x] Unit tests (12): signature valid/tampered/wrong-secret, event mapping,
      dispatch outcomes, handler status codes + dispatch invocation
- [ ] (Not yet) Upstream-drift poll loop (the app cannot subscribe to upstream,
      only to forks it is installed on)

**Up next**

- [ ] Stack cascade-rebase (behind `Rebase` trait; gix rebase is "idea" stage)
- [ ] Upstream-drift poll loop; wire webhook reconcile to the full engine
      (install token + local mirror + `reconcile`)

## Project layout

```
src/
  lib.rs           — library root
  config.rs        — fork config (upstream, fork, mirror branch derivation)
  webhook.rs       — GitHub webhook signature verification + event dispatch
  github/
    mod.rs         — module root + re-exports
    discovery.rs   — open-PR stack discovery (ordering logic) + live PR fetch
    auth.rs        — GitHub App identity, installation client + token
  engine/
    mod.rs         — git engine root
    sync.rs        — fast-forward mirror ref + sync_mirror orchestration
    fetch.rs       — fetch upstream / PR-head refs over the git transport
    stack.rs       — artifact composition (path changes + branch-stack overlay)
    pipeline.rs    — reconcile: config-derived sync + compose in one pass
  main.rs          — binary: load config, run the webhook server
```

## Development

```bash
cargo build
cargo test
cargo clippy
```

### Run the webhook server

```bash
FORK_MAINTAINER_ADDR=127.0.0.1:3000 \
  FORK_MAINTAINER_CONFIG=/path/to/config.json \
  cargo run
```

`config.json` is an [`AppConfig`](src/config.rs) with a `webhook_secret` and the
forks to maintain. GitHub is configured to POST events to `/api/webhook`;
signatures are verified before anything is dispatched.

Tests run against temporary bare repositories in the system temp dir, and
fetches use the local `file://` transport. No network or GitHub tokens
required for the current milestone.
