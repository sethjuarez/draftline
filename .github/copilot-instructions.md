# Copilot / agent instructions for Draftline

These instructions apply to every automated or AI-assisted change in this repository.

## Commit messages are MANDATORY Conventional Commits

**Every commit that lands on `main` MUST follow the
[Conventional Commits](https://www.conventionalcommits.org/) specification.** This is not a
style preference: releases and version bumps are computed automatically by
[release-please](https://github.com/googleapis/release-please-action) from commit history, so a
non-conforming message either ships the wrong version or is silently dropped from the changelog.

Because pull requests are squash-merged, **the PR title MUST also be a valid Conventional
Commit** — it becomes the commit on `main`.

### Required format

```
<type>[optional (scope)][optional !]: <description>

[optional body]

[optional footer(s)]
```

### Allowed types

| Type | Use for | Release effect (pre-1.0) |
| --- | --- | --- |
| `feat` | a new feature | minor |
| `fix` | a bug fix | patch |
| `docs` | documentation only | none |
| `refactor` | code change that neither fixes a bug nor adds a feature | none |
| `perf` | performance improvement | patch |
| `test` | adding or correcting tests | none |
| `build` | build system or dependency changes | none |
| `ci` | CI/workflow changes | none |
| `chore` | maintenance that doesn't touch `src`/published code | none |
| `style` | formatting only, no code meaning change | none |
| `revert` | reverts a previous commit | none |

The PR title is validated in CI by the **PR Title** workflow
(`.github/workflows/pr-title-lint.yml`); a non-conforming title fails the check and blocks merge.

### Breaking changes

Append `!` after the type/scope **and/or** add a `BREAKING CHANGE:` footer, e.g.
`feat(merge)!: change resolver registration signature`.

### Scopes

Prefer a scope that names the affected package or area, e.g. `merge`, `workspace`, `client`,
`react`, `site`, `ci`. Example: `fix(client): guard against empty version id`.

### Examples

- `feat(merge): let hosts inject semantic merge resolvers`
- `fix(workspace): enforce full 40-char hex in VersionId`
- `ci: sync workspace lockfile and add node-workspace plugin`
- `docs: document the release-please flow`

## Build & test

- Rust: `cargo test -p draftline` (crate) or `cargo test --workspace`. Lint with
  `cargo clippy --workspace --all-targets -- -D warnings`.
- JS/TS: `npm test` and `npm run build` from the repo root (npm workspaces).

## Releases (do not hand-edit versions)

- **Never manually bump** `version` in `crates/draftline/Cargo.toml` or any `package.json`, and
  never hand-edit a `CHANGELOG.md`. release-please owns all of these via the aggregated
  `chore: release main` PR.
- When package versions change, the root `package-lock.json` must stay in sync (the
  `node-workspace` release-please plugin handles this) or `npm ci` breaks in CI/Pages/publish.
- `@draftline/workbench` and `workbench/src-tauri` are intentionally **not** release-managed —
  don't add them to release-please config.

See the "Releases" section of `README.md` and `CONTRIBUTING.md` for the full flow.
