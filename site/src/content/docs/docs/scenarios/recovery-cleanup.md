---
title: Recovery and cleanup scenarios
description: Shelves, variation delete, support refs, purge planning, binary assets, interruption, and stale locks.
tableOfContents:
  minHeadingLevel: 2
  maxHeadingLevel: 3
---

> **Scenario lens:** These flows reduce clutter while preserving recoverability unless a separate purge workflow is chosen.

## Flow 12a: apply shelved work

Business goal: "Bring my put-aside work back into the current workspace."

Why this flow exists: applying a shelf is not always a simple file copy. The workspace may have moved forward since the shelf was created, so applying shelved work can conflict with current files just like teammate changes can.

```mermaid
flowchart TD
    A[User chooses shelf] --> B[preview_shelf]
    B --> C[preflight_apply_shelf]
    C --> D{Conflicts or collisions?}
    D -- No --> E[apply_shelf]
    D -- Yes --> F[Use semantic conflict UI]
    F --> G[Create resolved save or keep shelf]
```

| Question | Answer |
|---|---|
| Covered today? | Yes for all-work shelf list, preview, clean apply, and delete. No for selected-file shelves or conflict-resolution apply. |
| Current support | `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, and `delete_shelf` are public. Switch-with-shelve also creates shelf refs. |
| Safety behavior | Shelf application is treated as a file-writing operation with dirty-work and target-tree collision preflight. It preserves the shelf until the user explicitly deletes it. |
| Edge cases | Shelf names must be unique/create-only. Applying a shelf can conflict with current tracked work or collide with untracked/ignored/excluded files. Conflict-resolution apply and selected-file shelves remain future work. |
| Gap | Need selected-file shelves and semantic conflict-resolution apply for shelves that cannot be applied cleanly. |

## Flow 13: remove or clean up work

Business goal: "Hide old options without losing the ability to recover."

Why this flow exists: cleanup should reduce clutter while preserving a recoverable support ref to the old state before any visible ref is deleted. Draftline never rewrites existing history: commits already saved stay on the branch, so developers working on the same repository with plain Git are never asked to reconcile rewritten history.

```mermaid
flowchart TD
    A[User deletes a variation] --> C[Read variation tip]
    C --> D[Archive under refs/draftline/deleted-variations]
    D --> E[Delete variation branch]
    E --> J[Recovery possible from hidden support ref]
```

| Question | Answer |
|---|---|
| Covered today? | Covered for local delete. |
| Current support | `delete_variation` preserves the old tip under a local `refs/draftline/...` support ref before deleting the visible branch. Graph nodes expose whether a version is reachable from local, remote-tracking, or support refs so hosts do not treat recovery/remote reachability as normal timeline eligibility. |
| Safety behavior | The old commit remains named by a Draftline support ref locally, so it is not left only to reflog or garbage-collection timing on that machine. |
| Edge cases | Deleting the current variation is rejected. |
| Gap | Product UI still needs clear language for where deleted variations go and how to restore them. |

## Flow 13b: remove shared work

Business goal: "Remove an old option for everyone, not just on my machine."

Why this flow exists: local archive-before-delete is not enough for collaboration. If the visible variation was already published, Draftline must make the recovery point durable in the shared remote before deleting the shared visible ref.

```mermaid
flowchart TD
    A[User requests shared cleanup] --> B[Fetch visible refs and support refs]
    B --> C{Remote visible tip matches expected tip?}
    C -- No --> D[Stop and show teammate changed it]
    C -- Yes --> E[Create unique immutable support ref]
    E --> F[Publish support ref with create-only semantics]
    F --> G{Support ref durably published?}
    G -- No --> H[Stop; keep shared visible ref unchanged]
    G -- Yes --> J[Delete remote visible ref with expected-OID lease]
    J --> L[Fetch/prune and report result]
```

| Question | Answer |
|---|---|
| Covered today? | Covered for shared variation delete. |
| Current support | `preflight_delete_remote_variation` / `delete_remote_variation` archive the remote tip under a support ref, publish that support ref, then delete the visible remote variation. Draftline does not replace or force-push shared history. |
| Safety behavior | Shared remote cleanup is archive-first and visible-ref-last. Visible ref deletion is guarded by expected remote OID / lease checks, and support-ref publication is create-only. |
| Edge cases | Support-ref push can succeed while visible cleanup fails; that is safe but leaves an extra recovery point. Visible shared cleanup must not happen if support-ref publication fails. A stale visible remote tip must stop the delete instead of discarding teammate work. |
| Gap | Need protected-branch/server-capability diagnostics and teammate-facing product copy for hosts that reject support-ref namespaces or lease-protected deletes. |

## Flow 13c: sync hidden recovery support refs

Business goal: "Make recovery points for shared work available from another machine without showing them as normal variations."

Why this flow exists: the shared remote represents shared work. Support refs should travel with that shared work so cleanup recovery does not depend on a specific machine, while still staying out of normal business views.

```mermaid
flowchart TD
    A[Draftline creates refs/draftline support ref] --> B[Publish support refs to shared remote]
    B --> C[Another machine fetches support refs]
    C --> D[Recovery/admin view lists support refs]
    D --> E[Restore selected support ref as visible variation]
```

| Question | Answer |
|---|---|
| Covered today? | Covered for create-only support-ref publish, fetch, list, and restore. |
| Current support | Variation delete creates support refs under `refs/draftline/...`; support refs created by older Draftline versions under `refs/draftline/rewrites/squash/...` and `refs/draftline/backups/history-cleanup/...` are still listed and restorable. `preflight_publish_support_refs`, `publish_support_refs`, `fetch_support_refs`, `list_support_refs`, `restore_support_ref_as_variation`, `preflight_expire_support_refs`, and `expire_support_refs` cover support-ref lifecycle. `fetch_remote` also fetches support refs. |
| Safety behavior | Recovery support refs are hidden from normal views but are part of the shared repository trust boundary once synced. They are not privacy or access-control boundaries. |
| Edge cases | Remote support refs must be uniquely named, append-only, and fetched without overwriting unsynced local support refs. Hosts must surface remotes that reject `refs/draftline/...` pushes. |
| Gap | Remote support-ref retention remains local/admin oriented; hosted product UI still needs clear "hidden but shared" language. |

## Flow 13d: recover cleanup after clone or device loss

Business goal: "I deleted something on one machine and need it back elsewhere."

Why this flow exists: if support refs sync to the shared remote, recovery of shared work can follow the user across machines without adding a separate backup remote.

```mermaid
flowchart TD
    A[Fresh clone or second machine] --> B[fetch_support_refs]
    B --> C{Support ref exists?}
    C -- Yes --> D[list_archived_refs]
    D --> E[restore_archived_ref_as_variation]
    E --> F[Publish restored visible variation if desired]
    C -- No --> G[No shared recovery point available]
```

| Question | Answer |
|---|---|
| Covered today? | Covered when the cleanup support ref was published to the shared remote. |
| Current support | Local and remote-tracking archive refs can be listed and restored as a visible variation. |
| Safety behavior | The intended model is shared hidden support refs: recoverability travels with the shared remote, while normal views remain uncluttered. |
| Edge cases | Restoring an archived ref must create a new visible variation by default, require a non-conflicting name, fetch before publish, and never overwrite an existing local or remote variation without a separate explicit replace workflow. |
| Gap | Remote retention/expiration policy for old shared recovery points remains future work. |

## Flow 13f: permanently purge or redact content

Business goal: "I accidentally saved sensitive content; remove it permanently."

Why this flow exists: archive-first safety intentionally retains old content. If support refs are synced to the shared remote, purge/redaction must include every reachable ref namespace and clear limits about distributed clones.

```mermaid
flowchart TD
    A[User requests permanent removal] --> B{Normal cleanup?}
    B -- Delete variation --> C[Content retained in archive refs]
    B -- True purge/redaction --> D[Not covered today]
    D --> E[Need explicit destructive workflow]
```

| Question | Answer |
|---|---|
| Covered today? | Planning-only. `preflight_purge_content` and `verify_purge` exist, but there is no destructive purge execution. |
| Current support | Cleanup preserves old tips by design. `preflight_purge_content` enumerates candidate refs and reports distributed-Git limitations; `verify_purge` reports verification limitations. There is no destructive `purge_content` execution primitive. Planned support-ref sync would also preserve those tips on the shared remote. |
| Safety behavior | Draftline currently optimizes for recoverability, not permanent deletion. Purge must be a destructive, admin-permissioned, best-effort workflow over controlled remotes; Git cannot guarantee removal from existing clones, forks, backups, logs, caches, or offline devices that already fetched the objects. |
| Gap | Need destructive `purge_content` execution with confirmations, enumeration of visible refs, support refs, tags, notes, replace refs, stash refs, remote-tracking refs, reflogs, alternates, hosting caches, object reachability checks, remote GC coordination, post-purge verification, audit trail, and user copy that does not over-promise deletion from distributed copies. |

## Flow 13g: expire old support refs

Business goal: "Clean up old recovery points without pretending it is a sensitive-data purge."

Why this flow exists: shared support refs improve recovery but can grow indefinitely. Retention cleanup is a normal repository maintenance scenario, distinct from purge/redaction.

```mermaid
flowchart TD
    A[Admin opens support-ref retention view] --> B[List candidate support refs]
    B --> C[Show source operation, old tip, age, and restore impact]
    C --> D{Confirm expiration?}
    D -- No --> E[Keep support refs]
    D -- Yes --> F[Delete selected support refs]
    F --> G[Explain object GC and clone limitations]
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered locally. |
| Current support | `list_support_refs`, `preflight_expire_support_refs`, and `expire_support_refs` support local retention cleanup. General remote support-ref retention is not implemented. |
| Safety behavior | Retention cleanup may remove convenient recovery pointers but should not be framed as sensitive-content deletion. |
| Gap | Need remote support-ref retention with permissions, audit, and remote GC guidance. |

## Flow 13h: large or binary business assets

Business goal: "Save and share images, videos, PDFs, or other heavy assets safely."

Why this flow exists: creative/business content often includes binary assets, but Git history can grow quickly and text merge/diff tools do not apply.

```mermaid
flowchart TD
    A[User adds asset] --> B[changed_files]
    B --> C{Binary or large?}
    C -- No --> D[Normal save/publish]
    C -- Yes --> E[Report binary_files or large_files]
    E --> F{Host policy}
    F -- Warn --> D
    F -- Block --> G[Ask user to externalize]
    F -- External storage --> H[Not covered today]
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | Draftline detects binary and large current workspace files in change/preflight reports. Historical diffs do not preserve meaningful `is_large` data. |
| Safety behavior | Detection gives hosts a chance to warn or block, but Draftline does not enforce asset policy by itself. |
| Gap | Need a product policy for block/warn/stream/external storage/LFS-like behavior. |

## Flow 14: recover from interruption or unusual state

Business goal: "Something was interrupted or the workspace looks wrong. Help me get back safely."

Why this flow exists: interrupted ref-moving or file-writing operations can leave history, refs, and files temporarily inconsistent; the app should surface that state instead of continuing as if everything is normal.

```mermaid
flowchart TD
    A[App loads workspace] --> B[workspace_summary]
    B --> C{recovery present?}
    C -- No --> D[Normal workspace UI]
    C -- Yes --> E[Recovery prompt]
    E --> F{Operation type}
    F -- Switch variation --> G[Explain interrupted switch]
    F -- Restore version --> H[Explain interrupted restore]
    F -- Shelve changes --> I[Explain interrupted shelf]
    F -- Apply incoming --> J[Explain interrupted update]
    F -- Discard --> K[Explain interrupted discard]
    F -- Delete variation --> L[Explain archived ref]
    G --> M[Acknowledge, repair, or rollback]
    H --> M
    I --> M
    J --> M
    K --> M
    L --> M
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | Operation locks prevent concurrent risky operations. `RecoveryState` blocks normal APIs. `workspace_summary` and `inspect` can still surface recovery context. `repair_recovery` and `rollback_recovery` perform metadata-backed repair or rollback for covered operations such as discard, apply incoming, rename, and remote delete, and return typed blockers when the ledger lacks enough state. Both also smudge filtered files (Git LFS) that an interrupted checkout left as the exact pointer `HEAD` stores. A restore killed after its save landed (the branch already holds the restored content) is finished by repair, which writes the rest of the files and leaves the index matching. |
| Crash-point testing | The `fault-injection` cargo feature (test builds only; never ship it) adds deterministic crash points. Operations are named by the lock they take (`save_version`, `switch_variation`, `restore_version_as_new_save`, `discard_changes`, `apply_incoming`, `publish_changes`, `send`, `move_folder`, `repair_recovery`, `rollback_recovery`, ...); `draftline::fault::POINTS` lists the steps. `DRAFTLINE_FAULT_TRACE=<file>` appends `<operation>/<point>#<n>` for each step reached; `DRAFTLINE_FAULT=<operation>/<point>[#n]` (`*` for any operation) aborts the process the `n`th time that step is reached. A harness traces an operation once, then reruns it in a fresh process for each traced line, and after each crash reopens the workspace and runs the host's recovery: `repair_recovery`, then `rollback_recovery` if repair doesn't complete, then `acknowledge_recovery`. A crash with no unfinished recovery record leaves only the dead process's operation lock, which `clear_stale_lock` removes (repair, rollback, and acknowledge clear it themselves). The `refs_locked` point crashes a save while it holds `HEAD.lock` and the branch's ref lock. |
| Safety behavior | Draftline avoids pretending the workspace is coherent when an operation may have been interrupted. |
| Edge cases | `acknowledge_recovery` clears metadata but does not repair or roll back the Git state. Hosts should not present acknowledgment as repair; it can unblock normal APIs while refs and files remain inconsistent. Recovery state is single-slot because only one Draftline risky operation should hold the operation lock at a time. |
| Records from removed features | Records left by 0.5 milestone merge, squash, or history cleanup are not errors: repair and rollback return `completed: false` with `RepairRecovery` as the only next action, and change nothing. `acknowledge_recovery` (Rust, Tauri `acknowledge_recovery`, client `acknowledgeRecovery`) is the supported exit. Unreadable 0.5 pending-cleanup records are skipped, not reported as errors. |
| Unpublished 0.5 history cleanups | 0.5 kept these in `<gitdir>/draftline/history-cleanup/pending.json`. `workspace_summary.legacy_history_cleanups` lists them (omitted when empty). `dismiss_legacy_history_cleanup(plan_id)` (client `dismissLegacyHistoryCleanup`) drops the record; it never moves a branch or touches files. The original history stays in the cleanup's backup refs, which are listed as support refs, so the host can bring it back with `restore_support_ref_as_variation`. |
| Tests | `repair_recovery_finishes_discard_changes`, `repair_recovery_completes_apply_incoming_fast_forward`, `rollback_recovery_restores_deleted_variation`, `repair_remote_delete_recovers_after_visible_ref_was_deleted`, `recovery_records_from_removed_features_still_open_and_can_be_cleared`, `dismissing_an_unpublished_0_5_cleanup_moves_nothing_and_keeps_its_backup`, `unreadable_0_5_cleanup_records_are_skipped`, `tauri_contract_acknowledges_recovery_that_cannot_be_repaired`, `recovering_a_switch_killed_before_its_smudge_writes_real_content`, the crash-point walks in `crates/draftline/tests/fault_injection.rs` (`a_switch_recovers_from_a_crash_at_every_point`, `a_save_keeps_the_edit_through_a_crash_at_every_point`, `a_restore_recovers_from_a_crash_at_every_point`, `a_discard_recovers_from_a_crash_at_every_point`, `a_publish_can_be_retried_after_a_crash_at_every_point`, `a_git_lfs_switch_recovers_real_content_from_a_crash_at_every_point`), and related insufficient-metadata blocker tests. |
| Gap | Need broader operation metadata coverage so every interrupted risky operation can either repair, roll back, or explain why manual intervention is required. |

## Flow 14a: out-of-band Git mutation

Business goal: "Something changed outside the app; explain whether Draftline can continue safely."

Why this flow exists: users or tools may run Git directly, checkout detached commits, rewrite refs, resolve conflicts, or add commits outside Draftline.

```mermaid
flowchart TD
    A[External Git mutation] --> B[workspace_summary or current_variation]
    B --> C{Normal variation?}
    C -- Yes --> D[Continue with current state]
    C -- No --> E[NoCurrentVariation or Git error]
    E --> F[Need structured diagnostic and repair]
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | Some states surface through `NoCurrentVariation`, status, `inspect`, `verify_workspace`, `explain_error`, or raw Git errors. |
| Safety behavior | Draftline refuses normal variation operations when HEAD is detached or no local variation can be identified. |
| Tests | `tauri_contract_keeps_frontend_json_shape_stable` covers structured inspection output; lower-level error-path tests cover detached/no-current-variation signals. |
| Gap | Need deeper repair flows for detached HEAD, raw Git branch changes, existing conflicted indexes, and non-Draftline history edits. |

## Flow 14b: stale or abandoned operation lock

Business goal: "The app crashed or was killed; help me unlock the workspace without corrupting it."

Why this flow exists: an operation lock protects against concurrent risky mutations, but a crashed process can leave an abandoned lock behind. Retrying forever is not a recovery strategy.

```mermaid
flowchart TD
    A[Mutating operation returns WorkspaceLocked] --> B{Lock owner still alive?}
    B -- Yes --> C[Wait or show operation in progress]
    B -- No or unknown --> D[Read recovery state and workspace diagnostics]
    D --> E{Safe stale-lock repair?}
    E -- Yes --> F[clear_stale_lock or repair_recovery]
    E -- No --> G[Block and ask for admin/manual intervention]
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | `WorkspaceLocked` blocks mutating operations. `inspect_operation_lock` distinguishes active, stale, and legacy/unknown locks. `clear_stale_lock` clears only metadata locks deemed stale: the locking process is gone on this machine, or 30 minutes have passed (another machine or OS, such as Windows and WSL on one checkout, a lock that never recorded its machine, or the process can't be checked). When the lock's process is known to be gone (it ran on this machine and isn't running; age alone never counts), `clear_stale_lock` first removes the ref locks (`HEAD.lock` and the branch's `refs/heads/<branch>.lock`) that process left while moving `HEAD`, which would otherwise fail every later save with `GitIndexLocked`. A save records which lock files it is about to take, and which files it took, before it moves `HEAD`, and only those exact files are removed (a lock it died before recording must also have appeared after the record and still be there two seconds later, since Git holds a ref lock for milliseconds). A live Git command's lock, or any other lock, is never touched. `RecoveryState` may also exist, but acknowledgment does not repair refs/files. |
| Safety behavior | A host must distinguish an active operation from a stale lock. Clearing a lock should be a guarded recovery action, not a blind retry loop or automatic delete. |
| Edge cases | A crash can leave both recovery metadata and an operation lock. Multiple host instances may race on the same workspace. Lock metadata needs owner, PID/process identity, timestamp, and enough context to decide stale vs active. |
| Tests | `clear_stale_lock_removes_only_stale_metadata_lock`, `clear_stale_lock_removes_the_ref_locks_its_dead_holder_left`, `a_stale_lock_known_only_by_age_leaves_ref_locks_alone`, `a_lock_from_a_crashed_process_clears_right_away`. |
| Gap | Need integrated repair that coordinates lock and recovery state together. |

## Edge and error scenarios

| Scenario | Status | Primitive or signal | Expected host behavior |
|---|---|---|---|
| Invalid workspace-relative path | Covered | `AbsolutePath`, `PathEscapesWorkspace`, `resolve_path` | Show path validation error; do not retry with raw path. |
| Path outside content policy | Covered | `PathOutsideContentPolicy` | Explain that the file is app/runtime/private content, not tracked user content. |
| Invalid content policy path | Covered | `InvalidContentPolicyPath` | Fix host configuration. |
| Invalid content policy extension | Covered | `InvalidContentPolicyExtension` | Fix host configuration. |
| Invalid variation name | Covered | `InvalidVariationName` | Ask user for a different option name or generate a safe internal name. |
| Current variation delete requested | Covered | `CannotDeleteCurrentVariation` | Ask user to switch first or cancel. |
| Unknown version ID | Covered | `VersionNotFound` | Refresh history or report stale link. |
| Abbreviated or non-canonical version ID | Covered | `VersionId::from_canonical_string` | Require full lowercase canonical ID from app storage. |
| No current variation / detached state | Covered as signal | `NoCurrentVariation` | Show repair flow; do not run ref-moving operations. |
| Workspace locked | Covered as signal | `WorkspaceLocked` | Show active operation if known; if the lock may be stale, use the stale-lock recovery flow instead of retrying forever. |
| Stale operation lock | Partially covered | `inspect_operation_lock`, `clear_stale_lock` | Use guarded stale-lock repair; do not retry forever or delete locks manually. |
| Pending recovery | Covered as blocker | `RecoveryRequired` | Show recovery prompt instead of normal actions. |
| Dirty workspace before risky operation | Covered | `PreflightFailed` with `PreflightReport` | Ask user to save, discard, shelve, or cancel. |
| Target tree would overwrite non-versioned file | Partially covered | `FileHazard` checks in switch, restore, apply incoming, and apply shelf | Block checkout-like operation or ask user to move/backup the file. |
| Git-ignored file matches content policy | Partially covered | `policy_git_diagnostics`, `audit_content_policy` | Warn that business content may be hidden from save/publish. |
| Binary or large files in preflight | Covered | `binary_files`, `large_files` | Warn before switching or risky operation if useful. |
| Remote has incoming work during publish | Covered as blocker | `SyncNeedsMerge` containing `SyncState::IncomingAvailable` | Ask user to apply incoming work before publishing. |
| Remote needs merge | Covered as blocker | `SyncNeedsMerge` containing `SyncState::NeedsMerge` | Start merge workflow; do not publish/apply. |
| Remote/auth failure | Covered as error propagation | `Git` errors, `RemoteOptions` callbacks | Host should surface auth or network error and offer retry. |
| Unsupported switch discard | Covered as blocker | `UnsupportedSwitchPolicy` | Direct user to explicit discard flow, then switch with `AbortIfDirty`. |
