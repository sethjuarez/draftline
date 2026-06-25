# Recovery and cleanup scenarios

[Back to scenario index](../scenarios.md)

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
| Covered today? | Not covered. |
| Current support | Shelf refs can be created internally by switch-with-shelve, but there are no public list, preview, apply, delete, or shelve-in-place APIs. |
| Safety behavior | Shelf application should be treated as a merge-like file-writing operation, not as a guaranteed safe pop. It should preserve the shelf until the apply succeeds or the user explicitly deletes it. |
| Edge cases | Shelf names must be unique/create-only. Applying a shelf can conflict with current tracked work or collide with untracked/ignored/excluded files. Re-shelving with the same name should produce a business-shaped collision result, not a raw Git failure after partial mutation. |
| Gap | Need `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, and `delete_shelf`, plus conflict and naming contracts. |

## Flow 13: remove or clean up work

Business goal: "Hide old options and simplify the history without losing the ability to recover."

Why this flow exists: cleanup should reduce clutter while preserving a recoverable support ref to the old state before any visible ref is deleted or rewritten.

```mermaid
flowchart TD
    A[User chooses cleanup] --> B{Cleanup type}
    B -- Delete variation --> C[Read variation tip]
    C --> D[Archive under refs/draftline/deleted-variations]
    D --> E[Delete variation branch]

    B -- Squash recent versions --> F[Validate count and clean workspace]
    F --> G[Read current branch tip]
    G --> H[Archive under refs/draftline/rewrites/squash]
    H --> I[Move branch to squash commit]

    E --> J[Recovery possible from hidden support ref]
    I --> J
```

| Question | Answer |
|---|---|
| Covered today? | Partially covered. |
| Current support | `delete_variation` and `squash_versions` preserve old tips under local `refs/draftline/...` refs before destructive ref movement. |
| Safety behavior | The old commit remains named by a Draftline support ref locally, so it is not left only to reflog or garbage-collection timing on that machine. Cross-machine recovery still requires support-ref sync. |
| Edge cases | Deleting the current variation is rejected. Squash rejects dirty work, `count < 2`, and ranges without a parent outside the squash range. Squashing a published variation rewrites history, so normal `publish_changes` will reject it as diverged because Draftline does not force-push. |
| Gap | Need preflight APIs that explain what will be archived, support-ref sync to the shared remote, restore APIs that turn an archive ref back into a visible variation, and an explicit replace-remote-history workflow for shared squash. |

## Flow 13a: remove or rewrite shared work

Business goal: "Remove an old option or clean up history for everyone, not just on my machine."

Why this flow exists: local archive-before-delete is not enough for collaboration. If the visible variation was already published, Draftline must make the recovery point durable in the shared remote before deleting or replacing the shared visible ref.

```mermaid
flowchart TD
    A[User requests shared cleanup] --> B[Fetch visible refs and support refs]
    B --> C{Remote visible tip matches expected tip?}
    C -- No --> D[Stop and show teammate changed it]
    C -- Yes --> E[Create unique immutable support ref]
    E --> F[Publish support ref with create-only semantics]
    F --> G{Support ref durably published?}
    G -- No --> H[Stop; keep shared visible ref unchanged]
    G -- Yes --> I{Cleanup type}
    I -- Delete shared variation --> J[Delete remote visible ref with expected-OID lease]
    I -- Replace shared history --> K[Push replacement visible ref with force-with-lease]
    J --> L[Fetch/prune and report result]
    K --> L
```

| Question | Answer |
|---|---|
| Covered today? | Not covered. |
| Current support | `delete_variation` and `squash_versions` only operate on local refs and local archive refs. `publish_changes` refuses non-fast-forward history replacement. |
| Safety behavior | Shared cleanup must be archive-first, support-ref-publish-first, and visible-ref-delete-or-replace-last. The visible remote ref must only move if the old remote OID still matches the user's preflight. |
| Edge cases | Support-ref push can succeed while visible cleanup fails; that is safe but leaves an extra recovery point. Visible shared cleanup must not happen if support-ref publication fails. The user can still perform local-only cleanup, but not remote delete/replace, when shared recovery is unavailable. |
| Gap | Need `preflight_delete_remote_variation`, `delete_remote_variation`, `preflight_replace_remote_history`, and `replace_remote_history` or equivalent, with expected-OID/lease semantics and teammate-facing product copy. |

## Flow 13b: sync hidden recovery support refs

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
| Covered today? | Not covered. |
| Current support | Delete and squash create local support refs under `refs/draftline/...`. Publish/fetch currently use visible variation refs, not support refs. |
| Safety behavior | Recovery support refs are hidden from normal views but are part of the shared repository trust boundary once synced. They are not privacy or access-control boundaries. |
| Edge cases | Remote support refs must be uniquely named, append-only, and fetched without overwriting unsynced local support refs. Hosts must surface remotes that reject `refs/draftline/...` pushes. |
| Gap | Need `publish_support_refs`, `fetch_support_refs`, `list_archived_refs`, and `restore_archived_ref_as_variation` or equivalent. |

## Flow 13c: recover cleanup after clone or device loss

Business goal: "I deleted or squashed something on one machine and need it back elsewhere."

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
| Covered today? | Not covered. |
| Current support | Local archive refs may exist on the machine where cleanup happened. They are not pushed/fetched today. |
| Safety behavior | The intended model is shared hidden support refs: recoverability travels with the shared remote, while normal views remain uncluttered. |
| Edge cases | Restoring an archived ref must create a new visible variation by default, require a non-conflicting name, fetch before publish, and never overwrite an existing local or remote variation without a separate explicit replace workflow. |
| Gap | Needs support-ref sync and archive restore APIs. |

## Flow 13d: permanently purge or redact content

Business goal: "I accidentally saved sensitive content; remove it permanently."

Why this flow exists: archive-first safety intentionally retains old content. If support refs are synced to the shared remote, purge/redaction must include every reachable ref namespace and clear limits about distributed clones.

```mermaid
flowchart TD
    A[User requests permanent removal] --> B{Normal cleanup?}
    B -- Delete or squash --> C[Content retained in archive refs]
    B -- True purge/redaction --> D[Not covered today]
    D --> E[Need explicit destructive workflow]
```

| Question | Answer |
|---|---|
| Covered today? | Not covered. |
| Current support | Cleanup preserves old tips by design. Planned support-ref sync would also preserve those tips on the shared remote. |
| Safety behavior | Draftline currently optimizes for recoverability, not permanent deletion. Purge must be a destructive, admin-permissioned, best-effort workflow over controlled remotes; Git cannot guarantee removal from existing clones, forks, backups, logs, caches, or offline devices that already fetched the objects. |
| Gap | Need an explicit purge/redaction model with confirmations, enumeration of visible refs, support refs, tags, notes, replace refs, stash refs, remote-tracking refs, reflogs, alternates, hosting caches, object reachability checks, remote GC coordination, post-purge verification, audit trail, and user copy that does not over-promise deletion from distributed copies. |

## Flow 13e: expire old support refs

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
| Covered today? | Not covered. |
| Current support | No support-ref listing or deletion APIs. |
| Safety behavior | Retention cleanup may remove convenient recovery pointers but should not be framed as sensitive-content deletion. |
| Gap | Need `list_support_refs`, `preflight_expire_support_refs`, and `expire_support_refs` with permissions, audit, and remote GC guidance. |

## Flow 13f: large or binary business assets

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
    F -- Delete or squash --> L[Explain archived ref]
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
| Current support | Operation locks prevent concurrent risky operations. `RecoveryState` blocks normal APIs. `workspace_summary` can still surface recovery context. |
| Safety behavior | Draftline avoids pretending the workspace is coherent when an operation may have been interrupted. |
| Edge cases | `acknowledge_recovery` clears metadata but does not repair or roll back the Git state. Hosts should not present acknowledgment as repair; it can unblock normal APIs while refs and files remain inconsistent. Recovery state is single-slot because only one Draftline risky operation should hold the operation lock at a time. |
| Gap | Need operation-specific repair and rollback APIs. |

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
| Current support | Some states surface through `NoCurrentVariation`, status, or raw Git errors. |
| Safety behavior | Draftline refuses normal variation operations when HEAD is detached or no local variation can be identified. |
| Gap | Need a structured diagnostic report and repair flows for detached HEAD, raw Git branch changes, existing conflicted indexes, and non-Draftline history edits. |

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
| Covered today? | Not covered. |
| Current support | `WorkspaceLocked` blocks mutating operations. `RecoveryState` may also exist, but acknowledgment does not repair refs/files or clear an abandoned lock. |
| Safety behavior | A host must distinguish an active operation from a stale lock. Clearing a lock should be a guarded recovery action, not a blind retry loop or automatic delete. |
| Edge cases | A crash can leave both recovery metadata and an operation lock. Multiple host instances may race on the same workspace. Lock metadata needs owner, PID/process identity, timestamp, and enough context to decide stale vs active. |
| Gap | Need stale-lock detection and primitives such as `inspect_operation_lock`, `clear_stale_lock`, or integrated `repair_recovery` that handles lock and recovery state together. |

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
| Stale operation lock | Not covered | `WorkspaceLocked` only | Distinguish active work from abandoned lock; do not retry forever. |
| Pending recovery | Covered as blocker | `RecoveryRequired` | Show recovery prompt instead of normal actions. |
| Dirty workspace before risky operation | Covered | `PreflightFailed` with `PreflightReport` | Ask user to save, discard, shelve, or cancel. |
| Target tree would overwrite non-versioned file | Not covered | Missing shared preflight | Block checkout-like operation or ask user to move/backup the file. |
| Git-ignored file matches content policy | Partially covered | Current status behavior | Warn that business content may be hidden from save/publish. |
| Binary or large files in preflight | Covered | `binary_files`, `large_files` | Warn before switching or risky operation if useful. |
| Remote has incoming work during publish | Covered as blocker | `SyncNeedsMerge` containing `SyncState::IncomingAvailable` | Ask user to apply incoming work before publishing. |
| Remote needs merge | Covered as blocker | `SyncNeedsMerge` containing `SyncState::NeedsMerge` | Start merge workflow; do not publish/apply. |
| Remote/auth failure | Covered as error propagation | `Git` errors, `RemoteOptions` callbacks | Host should surface auth or network error and offer retry. |
| Not enough versions to squash | Covered | `InvalidSquashCount`, `NotEnoughVersionsToSquash` | Disable or explain squash action. |
| Unsupported switch discard | Covered as blocker | `UnsupportedSwitchPolicy` | Direct user to explicit discard flow, then switch with `AbortIfDirty`. |
