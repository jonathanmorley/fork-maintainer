# synthesize

A declarative branch synthesizer, delivered as a reusable GitHub Actions
workflow. Declare a base branch plus an ordered set of patch branches —
each possibly in a **different repository** — and get one synthesized output
branch, rebuilt from the base on every run.

```yaml
# in your fork, e.g. .github/workflows/sync.yml
jobs:
  synthesize:
    uses: jonathanmorley/fork-maintainer/.github/workflows/synthesize.yml@<sha>
    with:
      base: integrations/terraform-provider-github@main
      patches: |
        myorg/terraform-provider-github@fork-owned
        myorg/terraform-provider-github@feature-a
      output: myorg/terraform-provider-github@main
    secrets:
      token: ${{ secrets.SYNTH_TOKEN }}
    permissions:
      contents: write
```

Add your own triggers in the caller (`schedule` for upstream drift,
`push`/`pull_request` for fork-side changes, `workflow_dispatch` for manual
runs). Synthesis is idempotent: when the output already carries the composed
tree, the run is a quiet no-op.

## How it works

Each run, in an ephemeral runner with no persistent state:

1. **Fetch** the base branch and every patch branch (each from its own repo).
2. **Compose** base + patches in declared order. Default strategy `merge`
   does a three-way merge per layer with conflict detection; `overlay` is
   last-write-wins per path.
3. **Force-push** the single output commit to the output branch.

## Rules

- **The output branch is generated.** Never commit to it directly; every run
  rebuilds and force-pushes it. All persistent content lives in the base or
  a patch branch.
- **Conflicts fail loudly.** A conflicting patch fails the run *before*
  anything is pushed, naming the patch layer and the conflicted paths.
  Resolve on the patch branch (merge the base tip into it, or restack
  against lower layers for patch-on-patch overlap), push the patch, re-run.
  Nothing is ever pushed half-merged, and no conflict markers are committed.
- **One escape hatch.** `--strategy overlay` never fails on conflicts but can
  silently drop overlapping changes. Availability over correctness — declare
  it consciously.

## Tokens and permissions

- The caller must grant `contents: write`.
- The `token` secret defaults to `${{ github.token }}` in the caller, which
  is enough for public upstreams. Two limitations to know:
  - Pushes made with `github.token` **do not trigger downstream workflows**.
    Pass a PAT (or App token) as `token` if CI must run on the output.
  - Private upstreams need a token with read access to that repo (a fork's
    `github.token` cannot see someone else's private repo).

## CLI

The workflow wraps the `synthesize` binary in this repo (same interface):

```bash
synthesize --base integrations/repo@main \
  --patch myorg/repo@fork-owned \
  --patch other/repo@feature-x \
  --output myorg/repo@main \
  --strategy merge
```

`--config synthesis.json` is the file form (flags override it). Refs use
compact `owner/name@branch`. The token resolves as `--token`, then
`SYNTH_TOKEN`, then `GITHUB_TOKEN`. Exit 0 on success including no-ops;
non-zero with the error on stderr otherwise.

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets
cargo fmt --check
```

Engine tests run against temporary bare repositories with no network.
`src/engine` is the composition core (`stack` overlay, `rebase` strategies,
`fetch`, `push` via the git CLI, `pipeline` orchestration); `src/main.rs`
is the thin CLI; `src/config.rs` is the declarative spec.
