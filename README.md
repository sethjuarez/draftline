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

let workspace = Workspace::init("my-content")?;
let version = workspace.save_version("Client-ready draft")?;
let preview = workspace.preview_version(version.id())?;

println!("saved {} with {} files", version.label, preview.files.len());
# Ok::<(), draftline::DraftlineError>(())
```
