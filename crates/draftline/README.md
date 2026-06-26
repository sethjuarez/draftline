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
`merge_incoming_with_resolutions`, `publish_current_variation`). Collaboration
commands refresh remote-tracking state before reporting preflight results so host
UIs can render current `SyncState` values and then execute through Draftline's
tokenized apply, merge, and publish paths.

Conflicted merge preflight returns conflicts plus a token when the workspace and
remote heads are safe to merge, while `can_merge_cleanly` remains `false`. Hosts
should collect explicit user choices and call `merge_incoming_with_resolutions`
with the preflight token and one `MergeConflictResolution` per conflict. The
token binds execution to the local, remote, and merge-base commits the user
reviewed, so stale resolution submissions fail instead of resolving unseen
remote content. Whole-file choices support `use_ours`, `use_theirs`, `use_base`,
`delete`, or `use_content`; semantic `field_path` conflicts currently require a
host-produced `use_content` result for the resolved file.

The Workbench contract intentionally keeps credential handling out of its DTOs for
now for plain DTOs, but hosts can route commands through
`DraftlineCommandContext` to configure content policy, host-provided contributor
attribution, backend-only remote credentials, and redaction-safe workspace
events in one place.

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

For product hosts, prefer context-aware wrappers:

```rust,no_run
use draftline::{
    tauri_contract::{selected_save_with_context, DraftlineCommandContext, SelectedSaveRequest},
    ContentPolicy, Contributor, ContributorProfile,
};

let policy = ContentPolicy::new()
    .include("content")?
    .exclude(".chats")?
    .exclude("runtime")?;
let profile = ContributorProfile::new(
    Contributor {
        name: "Product User".to_string(),
        email: Some("user@example.invalid".to_string()),
    },
    Contributor {
        name: "Draftline Service".to_string(),
        email: Some("service@example.invalid".to_string()),
    },
);
let mut context = DraftlineCommandContext::new()
    .with_content_policy(policy)
    .with_contributor_profile(profile)
    .with_event_sink(|event| {
        // Tauri hosts can emit this as `draftline://workspace_event`.
        let _ = event.sequence;
    });
# let request = SelectedSaveRequest {
#     workspace_path: std::path::PathBuf::from("my-content"),
#     paths: vec![std::path::PathBuf::from("content/post.md")],
#     label: "Save".to_string(),
# };
let _ = selected_save_with_context(&mut context, request);
# Ok::<(), draftline::DraftlineError>(())
```
