# Draftline

Git-native versioning for creative content workflows.

Draftline is a Rust library for apps that need safe version history for folders full of creative content. It exposes business-friendly concepts like workspaces, versions, variations, change sets, preflight reports, and recovery state while keeping Git as a storage implementation detail.

## Content policy

Use a `ContentPolicy` to describe which workspace files are user content and which files are app/runtime state. Paths are workspace-relative, extensions are normalized case-insensitively, and `.draftline` state is excluded by default.

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

## Variation metadata

Variations have stable Draftline names backed by Git refs. Hosts can attach display metadata without changing those names.

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

`label` is user-facing display text. `slug` is host-owned metadata for URLs, routing, or integration. Neither field changes the underlying variation name or Git branch.

## Remote credentials

Remote operations accept credential callbacks so host apps can fetch, clone, and publish through their own authentication flow.

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

## Tauri Workbench contract

The `draftline::tauri_contract` module exposes dependency-free command adapter
functions for Workbench and other Tauri hosts. Hosts can wrap these functions with
`#[tauri::command]` while keeping Draftline's Rust APIs as the source of truth
for preflight, execution, verification, and serializable error shapes.

The contract includes read-only diagnostics (`inspect_workspace`,
`verify_workspace`, `list_variations`, `list_support_refs`), selected-file
mutations (`selected_save`, `selected_shelve`, `selected_discard`), and remote
collaboration commands (`fetch_remote`, `preflight_apply_incoming`,
`apply_incoming`, `preflight_merge_incoming`, `merge_incoming`,
`publish_current_variation`). Collaboration commands refresh remote-tracking
state before reporting preflight results so host UIs can render current
`SyncState` values and then execute through Draftline's tokenized apply, merge,
and publish paths.

The Workbench contract intentionally keeps credential handling out of its DTOs for
now. These remote commands are suitable for local, file, and otherwise
credential-free remotes; hosts that need private HTTPS or SSH authentication
should call the underlying workspace APIs with `RemoteOptions` until a
host-specific credential flow is added.

```rust,no_run
use draftline::tauri_contract::{inspect_workspace, WorkspaceRequest};

#[tauri::command]
fn inspect_workspace_command(
    workspace_path: std::path::PathBuf,
) -> draftline::tauri_contract::TauriCommandResult<
    draftline::tauri_contract::WorkspaceDiagnostics,
> {
    draftline::tauri_contract::into_tauri_result(inspect_workspace(WorkspaceRequest {
        workspace_path,
    }))
}
```
