# Draftline

Git-native versioning for creative content workflows.

Draftline is a Rust library for apps that need safe version history for folders full of creative content: posts, docs, demo plans, AI writing workspaces, prompt files, assets, and other project-shaped content.

It is not a Git wrapper for end users. Git is the storage layer. Draftline's public model is meant to feel closer to how a business or creative user thinks:

- save a useful version of the work
- see what changed
- try another direction without losing the current one
- recover from an earlier version safely
- share or back up the workspace

## MVP scope

The first crate focuses on an embeddable core:

- folder-backed workspaces
- safe path handling
- content policies that keep UI/runtime state out of saved versions
- versions backed by Git commits
- change sets, changed-file inspection, and risky-operation preflight reports
- variations backed by Git branches, without exposing detached-state workflows
- read-only version previews that do not mutate the live workspace
- recovery ledger metadata for multi-step operations
- structured merge conflicts with pluggable resolvers
- plain-text and lightweight Markdown/frontmatter merge proof points

Ghost publishing, CutReady-specific file formats, UI components, LLM provider logic, and CLI-first workflows are intentionally out of scope for the first pass.

See [Draftline scenarios](docs/scenarios.md) for the business-user flows, Mermaid diagrams, safe primitive mappings, and coverage gaps. See the [Draftline API plan](docs/api-plan.md) for the roadmap from those scenarios to Rust APIs, CLI commands, and agent/tool surfaces, then the [implementation plan](docs/implementation-plan.md) for the proposed engineering sequence.

## Working vocabulary

Draftline intentionally treats product language as design work, not just renamed Git commands. The current API uses:

- `Workspace` for the content folder
- `Version` for a named saved state
- `Variation` for an alternate direction
- `ChangeSet` for changed content
- `SwitchPolicy` and `PreflightReport` for risky workspace operations

These names are chosen to avoid common Git footguns such as detached states and destructive restores becoming normal product concepts.

## Example

```rust
use draftline::Workspace;

fn main() -> Result<(), draftline::DraftlineError> {
    let workspace = Workspace::init("my-content")?;
    let version = workspace.save_version("Client-ready draft")?;
    let preview = workspace.preview_version(version.id())?;

    println!("saved {} with {} files", version.label, preview.files.len());
    Ok(())
}
```

## Embedding examples

Apps can define content policies with workspace-relative roots, extensions, and exclusions. Extensions are normalized case-insensitively, and `.draftline` state is excluded by default.

```rust
use draftline::{ContentPolicy, Workspace};

fn main() -> Result<(), draftline::DraftlineError> {
    let policy = ContentPolicy::new()
        .include_paths(["content", "assets"])?
        .include_extensions(["md", "txt"])?
        .exclude_paths(["content/private"])?;

    let workspace = Workspace::init_with_policy("my-content", policy)?;
    Ok(())
}
```

Variations keep stable Draftline names backed by Git refs. Hosts can attach display metadata without changing those names: `label` is user-facing text, while `slug` is host-owned metadata for routing or integration.

```rust
use draftline::{VariationMetadata, Workspace};

fn main() -> Result<(), draftline::DraftlineError> {
    let workspace = Workspace::init("my-content")?;
    let version = workspace.save_version("Initial draft")?;
    let variation = workspace.create_variation_from_with_metadata(
        version.id(),
        "draft-a",
        VariationMetadata::new()
            .with_label("Draft A")
            .with_slug("draft-a"),
    )?;

    assert_eq!(variation.display_label(), "Draft A");
    Ok(())
}
```

Remote operations accept credential callbacks so host apps can use their own authentication flow.

```rust,no_run
use draftline::{RemoteCredential, RemoteOptions, Workspace};

fn main() -> Result<(), draftline::DraftlineError> {
    let token = std::env::var("GITHUB_TOKEN").unwrap();
    let mut options = RemoteOptions::new().with_credentials(move |request| {
        if request.allows_username_password {
            Ok(RemoteCredential::UsernamePassword {
                username: "x-access-token".to_string(),
                password: token.clone(),
            })
        } else {
            Ok(RemoteCredential::Default)
        }
    });

    let workspace = Workspace::open("my-content")?;
    workspace.fetch_remote_with_options("origin", &mut options)?;
    Ok(())
}
```

## Releases

Releases are automated with [release-please](https://github.com/googleapis/release-please-action).
Versioning and publishing are driven entirely by commit history, so **all commits to `main`
(and all squash-merged PR titles) MUST follow [Conventional Commits](https://www.conventionalcommits.org/)**
— this is mandatory, see [CONTRIBUTING.md](CONTRIBUTING.md):

- `fix:` &rarr; patch release
- `feat:` &rarr; minor release
- `feat!:` / `fix!:` / `BREAKING CHANGE:` footer &rarr; breaking release

### Managed packages

release-please tracks four independently versioned packages, each with its own
`CHANGELOG.md` and a component-prefixed tag:

| Package | Path | Type | Tag prefix | Published to |
| --- | --- | --- | --- | --- |
| `draftline` | `crates/draftline` | rust | `draftline-v*` | crates.io |
| `@draftline/client` | `packages/client` | node | `client-v*` | npm + GitHub Packages (`@sethjuarez/draftline-client`) |
| `@draftline/react` | `packages/react` | node | `react-v*` | npm + GitHub Packages (`@sethjuarez/draftline-react`) |
| `@draftline/site` | `site` | node | `site-v*` | GitHub Pages (continuous, no registry) |

The `@draftline/workbench` Tauri app and its `workbench/src-tauri` crate are intentionally
not managed by release-please.

### Flow

1. Commits land on `main` using Conventional Commit messages.
2. release-please opens/updates a single aggregated release PR that bumps versions and
   changelogs for the affected packages.
3. Merging that PR creates the component-prefixed tags and GitHub releases.
4. The `release-please` workflow then automatically publishes each package that was actually
   released: the crate via `cargo publish`, the npm packages to npmjs and GitHub Packages.
   The site is not registry-published — GitHub Pages already deploys on every push to `main`.

The manual `publish-crate` and `publish npm packages` workflows remain available via
`workflow_dispatch` as a fallback (type `publish` to confirm). Both paths keep the same
safety checks: they refuse to publish from a non-`main` ref and skip any version already
present on crates.io / npm.

