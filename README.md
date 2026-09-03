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
Each fork also declares `default_branch` (the knob the app maintains, default
`main`) and `local_mirror` (the local repo the app syncs and recomposes).

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

**Milestone 8 — Upstream-drift poll loop** ✅

The app is event-driven for *fork*-side changes (webhooks), but webhooks fire
only for repositories the app is installed on — an install on the fork never
surfaces *upstream* activity, even though the fork shares a fork network. So an
idle fork with no inbound events would never learn that upstream moved. This
milestone closes that gap with a poll loop.

- [x] `config::ForkConfig`: `default_branch` (the knob) and `local_mirror` (the
      local repo the app drives); `Repo::https_url()`
- [x] `poll::PollOutcome`: classify a reconcile as `NoChange` / `Changed` /
      `Failed`
- [x] `poll::run_pass`: iterate every fork, isolate per-fork failures (one bad
      fork never aborts the pass), classify each result
- [x] `poll::reconcile_fork`: open the configured `local_mirror` and run the
      engine `reconcile` (sync mirror + compose artifact)
- [x] `main.rs`: background poll task at a fixed interval (default 300s,
      `FORK_MAINTAINER_POLL_INTERVAL`), running reconcilies on a worker thread
      and logging whether anything changed
- [x] Unit tests (5 + 2 integration): classification of mirror
      advanced/up-to-date/diverged/error, per-fork error isolation, and
      `reconcile_fork` end-to-end over local bare repos

**Milestone 9 — full-engine live wiring** ✅ (webhook + poll drive discovery)

Both the webhook dispatcher and the poll loop now drive the *full* engine for a
fork: app auth -> live open-PR discovery -> PR-head fetch -> sync upstream ->
compose artifact.

- [x] `config::ForkConfig::fork_owned_branch`: the fork's persistent overlay
      branch (bottom stack layer); `Repo::authed_https_url(token)` for
      x-access-token-authenticated git access
- [x] `config::AppConfig::private_key_pem` + `credentials()` -> `AppCredentials`
- [x] `reconcile::reconcile_discovered`: the testable core — discover the
      ordered stack from PRs, fetch PR heads into the mirror, then `reconcile`
- [x] `reconcile::reconcile_fork_live`: async end-to-end — install client +
      token, `live_prs()`, then the blocking git phase on a worker thread
- [x] `main.rs`: webhook dispatcher and poll loop both call the live reconcile;
      missing credentials / local mirror => `Failed`, not a crash
- [x] Unit tests (2 integration): `reconcile_discovered` layers PRs + fork-owned
      over upstream over local bare repos (with and without a fork-owned branch)

**Milestone 10 — write side** ✅ (push recomposed artifact + mirror to fork)

The engine now completes the full cycle: after syncing the upstream mirror and
recomposing the artifact, it pushes both maintained refs back to the fork on
GitHub so users and CI see the updated artifact and PRs can target the mirror
ref.

- [x] `engine::push::push_refs`: push refspecs via the git CLI (gix 0.87.1 has
      no push API in its public API), parse porcelain output to extract pushed
      refs
- [x] `engine::push::push_fork_refs`: convenience for the two maintained refs —
      artifact (`<X>`) and mirror (`upstream/<X>`) pushed to the fork
- [x] `reconcile::reconcile_and_push_live`: full pipeline — authenticate,
      discover PRs, fetch, sync, compose, and push on a worker thread; push
      failures are logged separately without aborting the reconcile outcome
- [x] Unit tests (6): porcelain output parsing, empty refspec no-op, bad
      remote, single ref push, fork refs round-trip over local bare repos

**Up next**

- [ ] Stack cascade-rebase (behind `Rebase` trait; gix rebase is "idea" stage)

## Deployment (Render)

The app is containerized (see `Dockerfile`) and ships a Render blueprint
(`render.yaml`). It needs three things to function:

1. **A GitHub App** — create one, install it on the forks to maintain, and put
   the app's `app_id`, `private_key_pem`, and `webhook_secret` into config.
2. **Config** — a JSON `AppConfig` (see `src/config.rs`) exposed to the app via
   the `FORK_MAINTAINER_CONFIG` env var. Each `ForkConfig` needs a `local_mirror`
   path on the persistent disk (mount `/data/mirrors`, e.g.
   `/data/mirrors/<fork>`).
3. **Persistent disk** — Render web services reset their filesystem on deploy,
   so the local mirrors must live on a mounted disk. The app clones each fork's
   bare mirror on first boot (`mirror::ensure_mirror`) and reuses it
   thereafter; no manual `git clone` setup needed. It is concurrency-safe: a
   webhook and the poll loop firing for the same fork cannot double-clone.

### Health check

`GET /healthz` returns `200 OK` and is wired as Render's `healthCheckPath`.

### First deploy

1. `render.yaml` declares a `starter` web service with a 10 GB disk mounted at
   `/data/mirrors`, `FORK_MAINTAINER_ADDR=0.0.0.0:10000` (Render's expected
   port for web services), and a 300 s poll interval.
2. Set `FORK_MAINTAINER_CONFIG` to a secret env var (a JSON string is fine;
   the private key can be base64-decoded into the PEM on boot if needed).
3. Point GitHub's webhook at `https://<service>.onrender.com/api/webhook` with
   the app's webhook secret.
4. On first reconcile the app clones the fork mirrors, syncs upstream, composes
   the artifact, and pushes it back — the fork self-maintains from then on.

### Project layout

```
src/
  lib.rs           — library root
  config.rs        — fork config (upstream, fork, mirror branch derivation)
  webhook.rs       — GitHub webhook signature verification + event dispatch
  poll.rs          — upstream-drift poll loop (scheduling + outcome classify)
  reconcile.rs     — live reconcile orchestration (discovery + fetch + engine)
  mirror.rs        — first-boot bare-mirror clone into local storage
  github/
    mod.rs         — module root + re-exports
    discovery.rs   — open-PR stack discovery (ordering logic) + live PR fetch
    auth.rs        — GitHub App identity, installation client + token
  engine/
    mod.rs         — git engine root
    sync.rs        — fast-forward mirror ref + sync_mirror orchestration
    fetch.rs       — fetch upstream / PR-head refs over the git transport
    push.rs        — push recomposed artifact + mirror back to the fork (git CLI)
    stack.rs       — artifact composition (path changes + branch-stack overlay)
    rebase.rs      — Rebase trait: Overlay (current), Merge (3-way), CascadeRebase (future)
    pipeline.rs    — reconcile: config-derived sync + compose in one pass
  main.rs          — binary: load config, run the webhook server + poll loop
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
