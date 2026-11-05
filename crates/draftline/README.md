# Draftline

Git-native versioning for creative content workflows.

Draftline lets apps manage one part of a repo (the whole repo or a folder in it) for business users, without exposing Git. A committed root `.draftline` file names that folder.

The marker can also record project settings: a stable project `id` that survives `move_folder`, a `base_branch`, and a `commit_template` such as `docs(demo): {subject}` that Draftline applies to the first line of every commit it writes on a branch (saves, restores, merges, folder moves), so repos that lint commit messages accept them. Read them with `Workspace::project_settings` and change them with `Workspace::update_project_settings`, which saves only `.draftline`. `Workspace::init_project_in_repository` creates a new project folder (named by `Workspace::project_folder_name`) inside an existing repository with those settings, refusing when the repository already manages another folder.

To clone a particular branch or show progress, use `Workspace::clone_workspace_with` with `CloneOptions::with_branch` and `RemoteOptions::with_progress`, which reports `CloneProgress` through the receiving, resolving and checking-out stages, throttled to about ten reports a second. Clones are never shallow, so later updates can find a merge base. Tauri hosts register `clone_workspace_with_options` and forward the context's clone progress sink as `draftline://clone_progress`; `cloneWorkspaceWithOptions(request, onProgress)` in `@draftline/client` listens for them. `CloneOptions::with_blobless(true)` (`blobless: true` in the request) asks for a blobless clone, `git clone --filter=blob:none`: all of history, but file contents only for the checkout. It needs the `git` executable and falls back to a full clone without it, for SSH remotes, or when the host's credentials are anything but a username and password; `git` never prompts, and the configured credential helpers neither read nor store the host's login. libgit2 can't download missing contents, so reading an older version's file there can fail until `git` fetches it; `Workspace::is_partial_clone` tells you which kind you have.

For Send on GitHub, enable the optional `github` feature (`draftline = { version = "...", features = ["github"] }`). It adds `draftline::github::GitHubProvider`, a `PullRequestProvider` over the GitHub REST API (`GitHubProvider::for_remote_url(remote_url, token)`, or `for_enterprise_remote_url(remote_url, host, token)` for GitHub Enterprise Server; `with_head_owner(fork_owner)` opens pull requests from a fork into its parent), and `GitHubProvider::repo_access()`, which reports push permission, the default branch and the fork parent. The host owns sign-in and passes the token; Draftline never stores it. Calls block, so async hosts run them with `spawn_blocking`. Failures map to `github_unauthorized`, `github_no_push_access` (with `details.repository`, only when GitHub says the token can't push), `github_repository_not_found`, or `pull_request_failed`. Tauri hosts built with the feature can register `github_repo_access` (`GitHubRepoAccessRequest { remote_url, token }`, github.com only), which `githubRepoAccess(request)` in `@draftline/client` calls; for GitHub Enterprise Server the host calls `github_repo_access_on_enterprise_host(request, host)` with a host from its own settings, so the frontend can't choose where the token goes.

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

`merge_incoming` (and its `from_base: true` form) re-previews and merges whatever
is current, so a host that shows a preview and then asks the user to name the
merge should call `merge_incoming_with_token` instead, passing the clean token
from `preflight_merge_incoming` or `preflight_merge_base`. It refuses with
`remote_race` if the incoming head moved since that preview, and
`local_state_changed` if the current variation did. Conflicts still go through
`merge_incoming_with_resolutions`, which now reports the same two codes. For
local merges, `merge_variation_with_resolutions` with empty `resolutions` is the
token-pinned clean path.

`sync_status.ahead` counts only versions no remote-tracking branch already has
when the variation has no upstream yet (`no_remote_version`).

Merging one local variation into the current one follows the same shape without
a remote: `preflight_merge_variation` (read-only; `source_variation_id`) returns a
`MergeIncomingReport` whose `sync_status.variation` is the source and whose token
has `local: true`. `merge_variation` writes a clean merge; conflicts go through
`merge_variation_with_resolutions` with that token. The label is the full
version message (`"Name\n\nComment"`). The result is always a two-parent version,
even when a fast-forward was possible, so the merge stays a named step in
history. Errors: `preflight_failed` (unsaved changes, hazards, or unresolved
conflicts), `nothing_to_merge`, `local_state_changed` (either branch moved since
preflight), `invalid_merge_token` (a remote token on a local merge or the
reverse), `merge_needs_developer`, `variation_not_found`, and
`variation_outside_namespace` when a namespaced host names a branch outside it.

`preview_version_file` returns no content for binary files. To show an image as it
was in a version, call `read_version_file_bytes` (`version_id`, `path`, optional
`max_bytes`). It returns `VersionFileBytes { path, size, data_base64 }`: the raw
blob, with no checkout filters, so an LFS file comes back as its pointer. Like
`preview_version_file`, it follows moved folders and returns `None` for a missing
path, a directory, or a path the content policy doesn't track. A file over
`max_bytes` (default 20 MiB) fails with `file_too_large` and details
`{path, size, max_bytes}` before any bytes are read. The Rust
`Workspace::read_version_file_bytes` has no cap.

The Workbench contract keeps credential handling out of frontend DTOs. Hosts route
commands through `DraftlineCommandContext` to configure content policy,
host-provided contributor attribution, backend-only remote credentials, and
redaction-safe workspace events in one place. Clone, open, adopt, fetch, publish,
apply, and merge command adapters all use the context so product frontends never
need to pass clone or fetch tokens over IPC.

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

Events never fire while another Draftline operation on the same checkout holds
its operation lock (for example a merge, shelf apply, or switch still writing
files on another thread or process). The event waits for that operation to
finish, up to `with_event_defer_timeout` (default
`DEFAULT_EVENT_DEFER_TIMEOUT`, 30 seconds; after that it is emitted anyway).
If that operation emits its own event of the same kind in the same process,
the waiting event is dropped (unless it carries a sync status), so the host
sees one event, after the files are written.

Reads of unsaved work (`changed_files`, `changes`, `diff_version_to_workspace`,
`diff_workspace_file`) tolerate files that change or vanish mid-scan, as editors
that save atomically cause. They rescan for up to about 0.6 seconds; the
rescan reports the file's new state (changed, deleted, or gone). On the last
attempt a file still changing is left out of the patch text but stays listed.
Only a file rewritten continuously for longer than that surfaces the Git
error.

Treat `ContributorProfile` as product identity and service attribution, not as a
Git configuration screen. The host should derive `author` from the signed-in
product user, `saved_by` from the actor that initiated the save, and optional
`service` from the backend automation performing the operation. Draftline writes
normal Git author/committer metadata from that profile, but users should not need
to understand or edit `user.name` / `user.email` for product saves:

```rust,no_run
use draftline::{Contributor, ContributorProfile};

let profile = ContributorProfile::new(
    Contributor {
        name: "Avery Writer".to_string(),
        email: Some("avery@example.invalid".to_string()),
    },
    Contributor {
        name: "CutReady Sync Service".to_string(),
        email: Some("sync@example.invalid".to_string()),
    },
)
.with_service(Contributor {
    name: "CutReady".to_string(),
    email: None,
});
# let _ = profile;
```

When a host does want the developer's Git identity (for example as a fallback
author), read it through Draftline rather than linking `git2` directly.
`Workspace::git_identity()` honours repository config, and
`draftline::default_git_identity()` reads global config before a project is
open. Both return `Ok(None)` when no `user.name` is set and an error when Git
config cannot be read.

Every operation that writes a commit honors the profile: save, selected save,
merge, shelve (including `switch_variation` with `SaveFirst`/`Shelve`), restore
as a new save. On `Workspace`, use the `*_with_profile` variants, such as
`shelve_files_with_profile` and `restore_version_as_new_save_with_profile`. The
`*_with_context` contract functions pass the context profile automatically.
Methods without a profile keep using the Git config identity.

## Native mobile bridge

The `draftline::mobile_ffi` module exposes a small C ABI foundation for native
mobile hosts such as Swift/iOS apps. It keeps Draftline as the owner of workspace
versioning and sync semantics while giving the host an opaque workspace handle,
C-safe status/error results, a host-provided `ContentPolicy`, a credential
callback shape, and JSON return values for structured Draftline types.

The mobile bridge supports opening/initializing or cloning a workspace, reading
and writing policy-tracked workspace files, saving a version, local workspace
status, fetch, sync status, fast-forward apply incoming, guarded publish
preflight, tokenized publish, local shelf/unshelve workflows, and tokenized
incoming merge conflict recovery. Shelf functions accept `paths_json` as a JSON
array of workspace-relative paths, or `NULL` to target all dirty
policy-tracked files. Merge functions return the same conflict/token/resolution
JSON shapes as the Tauri/client contract so native hosts can preflight, render a
conflict view model, and submit explicit `MergeConflictResolution` JSON without
adding C structs for each DTO; mutating mobile functions return the underlying
Rust result DTOs rather than Tauri command postcondition wrappers. Product-specific
file types and content rules stay outside Draftline; mobile hosts pass their
policy at open/clone time.

Mobile remote operations are bounded natively so hosts do not have to rely on a
Swift/Java/Kotlin-side timeout while a synchronous FFI call remains blocked in
libgit2. `draftline_mobile_workspace_clone`,
`draftline_mobile_workspace_fetch_remote`,
`draftline_mobile_workspace_apply_incoming_json`,
`draftline_mobile_workspace_merge_incoming_json`,
`draftline_mobile_workspace_merge_incoming_with_resolutions_json`,
`draftline_mobile_workspace_preflight_publish_json`, and
`draftline_mobile_workspace_publish_json` apply a default 20 second native
network timeout. Each has a `_with_timeout` variant that accepts a trailing
`timeout_ms`; pass `0` only when the host intentionally wants to disable the
native timeout for that call. Timeout failures return
`DraftlineMobileStatusCode::RemoteTimeout`, authentication and credential
callback failures return `CredentialRejected`, and transport failures return
`RemoteNetwork` so mobile apps can map them to offline/retry UX.

For iOS integration, build Draftline as a Rust static library for the desired
Apple targets, generate/bind the C declarations, and wrap the opaque handle in a
small Swift type that frees returned strings and handles. Remaining integration
work is packaging rather than new Draftline semantics: target-specific
`libgit2`/OpenSSL or platform TLS linkage, XCFramework assembly, generated Swift
bindings, and app-owned credential storage/UI.

Draftline's `Build iOS package` GitHub Actions workflow is manual-only. Run it
with `confirm_build=build` to produce `DraftlineMobile.xcframework.zip`,
`draftline_mobile.h`, a checksum file, and SwiftPM manifest snippets as workflow
artifacts. Provide `release_tag` to publish those same files as GitHub Release
assets. The workflow builds device and simulator static libraries with
`vendored-mobile-git`, assembles an XCFramework, and prints the exact SwiftPM
checksum to the run summary. Mobile hosts consuming a binary target must bump the
XCFramework URL/tag and checksum together after a DraftlineMobile release.

## Workspace graph APIs

Hosts that render version history should prefer Draftline's `WorkspaceGraph`
helpers over reconstructing branch topology from raw Git refs. The graph is a
Draftline view of variation semantics: node IDs are opaque, refs are overlays,
and actions are expressed as Draftline commands instead of raw checkout
operations.

Use the bounded helpers by default:

- `workspace_graph_overview` for compact sidebar/history summaries.
- `workspace_graph_for_variation` for a focused variation lane.
- `workspace_graph_refs` for cheap badge/tip refreshes after saves, fetches, or
  switches.
- `workspace_graph_around_version`, `workspace_graph_neighborhood`,
  `search_workspace_graph`, `workspace_graph_path`, and
  `workspace_graph_common_ancestor` for focused expansion in large histories.

`workspace_graph` returns the full graph and should be opt-in for expanded
history views. Bounded and searched graphs can be partial; render
`was_pruned`, `has_more`, `next_cursor`, and each node's boundary metadata so
hidden parent/child edges are visible to users instead of looking like broken
rails. Layout hints are advisory and stable for a graph snapshot, but hosts
should treat them as hints and keep a fallback layout. Action hints carry stable
action identifiers, the Draftline command to call, whether the action creates a
version or switches the workspace, and a `disabled_reason` when an action is not
safe.
