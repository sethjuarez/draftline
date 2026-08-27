# Contributing to Draftline

## Conventional Commits are required

Draftline releases are fully automated with
[release-please](https://github.com/googleapis/release-please-action): version bumps,
changelogs, tags, and registry publishes are all derived from commit history. As a result,
**every commit on `main` — and therefore every pull request title (PRs are squash-merged) —
MUST be a valid [Conventional Commit](https://www.conventionalcommits.org/).**

Non-conforming messages are rejected in review because they produce an incorrect release or are
dropped from the changelog. This is enforced automatically: the **PR Title** GitHub Actions
check validates every pull request title against the Conventional Commits spec and **must pass
before a PR can be merged**.

### Format

```
<type>[optional (scope)][optional !]: <description>
```

**Types:** `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `style`, `revert`.

- `feat:` → minor release, `fix:` → patch release.
- Breaking changes: add `!` (e.g. `feat(merge)!: ...`) and/or a `BREAKING CHANGE:` footer.
- Use a scope naming the affected package/area where possible: `merge`, `workspace`, `client`,
  `react`, `site`, `ci`.

Examples:

- `feat(merge): let hosts inject semantic merge resolvers`
- `fix(client): handle missing dist-tag`
- `docs: clarify resolver precedence`

## Do not hand-edit versions or changelogs

Never manually change `version` in `crates/draftline/Cargo.toml` or a `package.json`, and never
edit a `CHANGELOG.md`. release-please manages these through its `chore: release main` PR.

## Before opening a PR

- Rust: `cargo test -p draftline` and `cargo clippy --workspace --all-targets -- -D warnings`.
- JS/TS: `npm test` and `npm run build`.

See the "Releases" section of the [README](README.md) for the end-to-end release flow.
