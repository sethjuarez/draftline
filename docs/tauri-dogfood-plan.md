# Tauri Workbench plan

Draftline should prove its host-facing contract in a real Tauri application, not only through Rust unit tests. The Workbench goal is an end-to-end harness that exercises Draftline through Tauri commands, React views, real filesystem state, and real remote Git state while Auditaur observes the app.

## Goals

- Validate Draftline as a safe Tauri subsystem for content apps.
- Exercise the same JSON/serde contracts a host app would use.
- Capture frontend actions, Tauri invokes, Rust spans, Git ref transitions, and recovery prompts in one observable timeline.
- Turn manual remote/recovery confidence checks into repeatable smoke scenarios.
- Discover API gaps before exposing reusable npm packages or React views.

## Workbench app

Use `workbench/` as the repo-local Tauri host for repeatable Draftline validation. Auditaur should observe Workbench runs rather than being the primary host, so Draftline owns a stable UI and command-contract fixture while Auditaur owns timeline, exception, trace, IPC, and bundle observation.

Use `packages/client/` for the typed TypeScript command client over the Workbench/Tauri invoke boundary and `packages/react/` for provider-backed hooks and reusable React components. Keep publishing gated until tarball consumer smoke, command-contract tests, and Workbench bridge runs pass.

## Harness shape

1. Add a small Tauri command adapter over Draftline APIs.
2. Add dev-only React views for workspace state and operation flows.
3. Run scenarios against disposable local workspaces and a disposable shared remote.
4. Query Auditaur after each run for exceptions, failed traces, failed IPC calls, and timeline detail.
5. Save a redacted Auditaur bundle for handoffs when a scenario fails.

## Next confidence phase: contract harness

Start with a dependency-free Rust command adapter that Tauri hosts can wrap with
`#[tauri::command]`. This keeps Draftline's Rust crate as the source of truth
for preflight, mutation, verification, and error semantics while giving the
Workbench app the same JSON/serde boundary that a real Tauri frontend will use.

Initial adapter commands should cover the first repeatable smoke loop:

- `inspect_workspace` returns a dashboard-ready diagnostics payload.
- `verify_workspace` returns postcondition checks after each scenario.
- `list_variations` feeds the variation switcher.
- `list_support_refs` feeds recovery/admin views.
- `selected_save`, `selected_shelve`, and `selected_discard` exercise selected-file flows.
- `publish_current_variation` exercises local-to-remote sharing with tokenized publish.

The first React panel should render the adapter output directly: workspace
summary, dirty files, current variation, remotes, recovery/lock state, support
refs, safe next actions, and a raw JSON inspector. Avoid reusable npm packaging
until the command payloads survive Workbench runs.

## Initial Tauri commands

- `inspect_workspace`
- `verify_workspace`
- `list_variations`
- `selected_save`
- `selected_shelve`
- `selected_discard`
- `publish_current_variation`
- `delete_remote_variation`
- `restore_support_ref`
- `squash_versions`
- `confirm_replace_remote_history`
- `repair_recovery`

## Initial React views

- Workspace summary and safe next actions.
- Dirty files and selected-file operations.
- Variation switcher and remote variation diagnostics.
- Support-ref list and restore flow.
- Recovery prompt with repair, rollback, and acknowledge actions.
- Destructive/shared-history confirmation panel.
- Raw JSON inspector for command input/output during Workbench runs.

## Auditaur workflow

Prefer attach mode when a developer owns app startup. Workbench initializes the Auditaur in-app drive bridge, so selector actions should use the bridge and should not require CDP:

```powershell
auditaur debug --app draftline-workbench --active --require-drive-bridge --require-frontend --json watch --until-ready --timeout-seconds 120
```

Prefer wrapper mode for repeatable smoke runs:

```powershell
auditaur debug --app draftline-workbench --active --require-drive-bridge --require-frontend --json run --timeout-seconds 180 -- npm run workbench:dev
```

After each scenario, inspect:

```powershell
auditaur errors --active --json
auditaur traces --active --json
auditaur ipc --active --json
auditaur explain --active --json
```

## Scenario backlog

1. Create/open a Draftline workspace and render diagnostics.
2. Save, shelve, and discard selected files with mixed staged and worktree state.
3. Publish a variation to a disposable remote.
4. Fetch and display remote-only and local-only variation diagnostics.
5. Delete a remote variation and verify hidden support refs.
6. Restore a support ref as a new visible variation.
7. Squash local history, require explicit confirmation, and replace shared history.
8. Simulate remote-delete crash windows and repair recovery from the UI.
9. Surface operation locks and stale-lock recovery prompts.
10. Export a redacted Auditaur bundle for a failed scenario.

## Scenario confidence matrix

| Scenario | Rust/API coverage | Tauri contract coverage | TypeScript/client coverage | React/package coverage | Bridge smoke coverage | Remaining gap |
|---|---|---|---|---|---|---|
| Open workspace diagnostics | `scenario_flows.rs` and workspace tests validate summaries, verification, and diagnostics. | `tauri_contract.rs` validates `inspect_workspace` JSON keys and support refs. | `@draftline/client` tests lock command name and request casing. | Provider lifecycle test loads diagnostics/support refs. | Workbench bridge inspect renders package-backed panels. | None for current package boundary. |
| Selected save/shelve/discard | Rust selected-file APIs and scenario tests validate postconditions. | `selected_save`, `selected_shelve`, `selected_discard` contract tests validate preflight/postcondition DTOs. | Client tests lock selected-operation request casing. | Selected-operation failure test keeps preflight errors visible. | Bridge smoke drove selected save, shelve, and discard through Workbench UI. | Rich selected-file conflict UI remains future work. |
| Remote fetch/apply | Collaboration scenario validates fast-forward incoming apply. | Contract tests validate `fetch_remote`, `preflight_apply_incoming`, and `apply_incoming`. | Client tests lock remote request casing. | `RemoteSyncBar` test validates fetch/preflight/apply lifecycle and refresh. | Bridge smoke applied teammate work through package-backed Workbench UI. | Broader remote lifecycle diagnostics remain future work. |
| Clean merge / conflict preflight | Collaboration scenario validates clean merge and conflict preflight without mutation. | Contract tests validate merge success and blocked merge serialized errors. | Client tests lock `merge_incoming` request casing. | Mutation failure tests cover visible error state. | No full bridge merge-conflict UI yet. | User-driven conflict resolution panel remains future work. |
| Content policy diagnostics | Content-policy scenario tests cover ignored/tracked diagnostics and large/binary signals. | Diagnostics are included in `inspect_workspace`/verification contract payloads. | DTOs expose verification diagnostics and dirty file large/binary fields. | `ContentPolicyDiagnosticsPanel` test renders warning diagnostics. | Workbench renders diagnostics panel after inspect. | Policy migration/redaction remains future work. |
| Recovery/support refs | Support-ref scenarios cover local and remote-tracking restore paths. | `list_support_refs` contract tests validate local support refs and stable shape. | Client tests lock support-ref command casing. | Provider and inspector tests load and render support refs. | Workbench bridge inspect shows support refs tab. | Remote retention policy remains future work. |
| Package consumption | N/A | N/A | `npm pack --dry-run` verifies built client exports. | `npm pack --dry-run` verifies built React exports and registry-compatible dependency metadata. | Separate tarball consumer smoke imports both packages outside Workbench. | Actual npm publishing is intentionally gated and not automated. |

## npm packages

The current package split is:

- `@draftline/client`: typed TypeScript client over Tauri invokes.
- `@draftline/react`: provider-backed hooks and reusable React views for diagnostics, recovery, variations, dirty files, remote sync, and command inspection.
- Future `@draftline/test-harness`: scenario helpers for Workbench-style apps and CI smoke runs.

These packages should remain host-agnostic and treat the Rust crate as the source of truth for safety decisions.
