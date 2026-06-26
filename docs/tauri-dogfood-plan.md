# Tauri dogfood plan

Draftline should prove its host-facing contract in a real Tauri application, not only through Rust unit tests. The dogfood goal is an end-to-end harness that exercises Draftline through Tauri commands, React views, real filesystem state, and real remote Git state while Auditaur observes the app.

## Goals

- Validate Draftline as a safe Tauri subsystem for content apps.
- Exercise the same JSON/serde contracts a host app would use.
- Capture frontend actions, Tauri invokes, Rust spans, Git ref transitions, and recovery prompts in one observable timeline.
- Turn manual remote/recovery confidence checks into repeatable smoke scenarios.
- Discover API gaps before exposing reusable npm packages or React views.

## Candidate dogfood app

Use an Auditaur-observed Tauri app, potentially Auditaur itself, as the first dogfood host. The app should start through its normal development workflow while Auditaur attaches in observe mode, or through wrapper mode for repeatable smoke runs.

## Harness shape

1. Add a small Tauri command adapter over Draftline APIs.
2. Add dev-only React views for workspace state and operation flows.
3. Run scenarios against disposable local workspaces and a disposable shared remote.
4. Query Auditaur after each run for exceptions, failed traces, failed IPC calls, and timeline detail.
5. Save a redacted Auditaur bundle for handoffs when a scenario fails.

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
- Raw JSON inspector for command input/output during dogfood.

## Auditaur workflow

Prefer attach mode when a developer owns app startup:

```powershell
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'
auditaur debug --app draftline-dogfood --active --cdp-port 9222 --json watch --until-ready
```

Prefer wrapper mode for repeatable smoke runs:

```bash
auditaur debug --app draftline-dogfood --active --cdp-port 9222 --json run --timeout-seconds 180 -- npm run tauri dev
```

After each scenario, inspect:

```bash
auditaur exceptions --json
auditaur traces --failed --json
auditaur ipc --failed --json
auditaur bundle --redacted --output auditaur-bundle.json
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

## Future npm packages

Once the Tauri command contract stabilizes, split reusable frontend pieces into npm packages:

- `@draftline/tauri-client`: typed TypeScript client and React hooks over Tauri invokes.
- `@draftline/react`: reusable React views for diagnostics, recovery, variations, dirty files, and confirmation flows.
- `@draftline/test-harness`: scenario helpers for dogfood apps and CI smoke runs.

These packages should remain host-agnostic and treat the Rust crate as the source of truth for safety decisions.
