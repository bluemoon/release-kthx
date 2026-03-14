# release-kthx

`release-kthx` is a release-plz-style automation tool designed primarily for private repositories.

It ships first as a GitHub Action (Docker action), with a Rust CLI runtime inside the action image.

## Why private-first

- No crates.io dependency in the default flow
- Conventional-commit based version planning from git history
- Works with org-private repos and custom GitHub tokens in CI
- PR-first release flow per crate: open release PR with crate version bumps, publish crate tags/releases after merge
- GitHub releases include generated notes from crate-relevant conventional commits since the prior version
- Private-workspace-aware internal dependency normalization (`auto`, `strip`, `update`)

## GitHub Action usage

```yaml
name: release-pr-and-publish

on:
  workflow_dispatch:
  push:
    branches: [main]

jobs:
  release_pr:
    runs-on: ubuntu-latest
    permissions:
      contents: write
      pull-requests: write
    steps:
      - name: Checkout
        uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Create or update release PR
        uses: ./. # replace with your published action ref
        env:
          GITHUB_TOKEN: ${{ github.token }}
        with:
          mode: release-pr
          path: .

  publish:
    needs: [release_pr]
    runs-on: ubuntu-latest
    if: github.event_name == 'push'
    permissions:
      contents: write
    steps:
      - name: Checkout
        uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Publish merged release
        uses: ./. # replace with your published action ref
        env:
          GITHUB_TOKEN: ${{ github.token }}
        with:
          mode: publish-on-merge
          path: .
          dry-run: "false"
          push: "true"
```

## Action inputs

- `mode`: `init` | `check` | `plan` | `release-pr` | `release` | `publish` | `publish-on-merge` (default `plan`)
- `path`: repository path (default `.`)
- `from-tag`: optional base tag (`plan` / `release-pr` / `release`)
- `base-branch`: base branch for `release-pr` (default `main`)
- `pr-branch`: branch for `release-pr` updates (default `release-kthx/release-pr`)
- `dry-run`: for `release` / `publish` / `publish-on-merge` (default `true`)
- `push`: for `release` / `publish` / `publish-on-merge`; pushes created tag to origin (default `true`)
- `force`: only for `init` (default `false`)

## Local CLI usage

```bash
cargo run -- init --path .
cargo run -- check --path .
cargo run -- plan --path .
cargo run -- release-pr --path . --base-branch main --pr-branch release-kthx/release-pr
cargo run -- release --path . --dry-run
cargo run -- publish --path . --dry-run
cargo run -- publish-on-merge --path . --dry-run
```

## Config file

`release-kthx.toml`:

```toml
[release]
tag_template = "{{ crate }}-v{{ version }}"
internal_dependency_policy = "auto"

[github]
create_release = true
token_env = "GITHUB_TOKEN"
repository_env = "GITHUB_REPOSITORY"
```

`internal_dependency_policy` controls how `release-kthx` handles internal workspace dependencies:

- `auto` (default): strip `version` from private-to-private internal deps, update existing version fields elsewhere
- `strip`: always remove `version` from internal path/workspace deps
- `update`: keep `version` fields and rewrite them when workspace members move

## This repository uses itself

This repo includes `release-kthx.toml` and `.github/workflows/self-release.yml` so it can dogfood the release flow:

1. open/update a release PR with bumped `Cargo.toml` versions,
2. human merges the PR,
3. publish job creates tags/releases per crate version (for crates changed in the release PR merge).

## Workspace layout

- `release-kthx` (root crate): application/infrastructure layer (CLI, git adapter, config, action wiring)
- `crates/release-kthx-domain`: domain layer with release planning model and versioning rules
