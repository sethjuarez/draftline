# Draftline implementation plan

This plan turns the scenario gaps in [coverage and roadmap](coverage.md) and the API shape in [api-plan](api-plan.md) into implementable engineering slices. The ordering favors safety foundations first, then collaboration and cleanup workflows that depend on those foundations.

## Guiding constraints

1. Mutating operations should have read-only inspection or preflight before execution.
2. Operation results should be business-shaped: blockers, warnings, safe next actions, and stable error/result codes.
3. File-writing operations should share target-tree collision detection.
4. Ref-deleting or ref-rewriting operations should preserve old tips under `refs/draftline/...` unless an explicit purge/redaction workflow is being executed.
5. Remote mutations should bind to expected remote identity and expected OID or expected absence.
6. Recovery, operation locks, support refs, and content policy diagnostics should be normal API concepts, not implementation details.

## Slice 1: workspace inspection and capabilities

**Goal:** give hosts and agents one structured view of workspace safety before choosing an action.

**Current status:** Implemented for the Rust API. `Workspace::inspect`, `Workspace::capabilities`, JSON helpers, stable diagnostics, and safe next actions exist.

| Work item | Outcome |
|---|---|
| Add `Workspace::inspect()` | Reports workspace ID, sharing mode, current variation, dirty state, remotes, content-policy diagnostics, recovery, lock state, support refs, and safe next actions. |
| Add `Workspace::capabilities()` | Reports which Draftline features exist in this crate version. |
| Introduce `SafeNextAction` and stable diagnostic codes | Lets apps and agents react without parsing prose. |

**Acceptance:** callers can determine whether normal work, recovery, remote setup, or user choice is required without running a mutating operation.

## Slice 2: recovery and stale-lock repair

**Goal:** make interrupted operations repairable instead of only acknowledgeable.

**Current status:** Partially implemented. Lock metadata, `inspect_operation_lock`, and guarded `clear_stale_lock` exist. `repair_recovery` and `rollback_recovery` are typed skeleton entry points that report the interrupted operation but do not yet perform operation-specific mutations.

| Work item | Outcome |
|---|---|
| Add lock metadata | Record owner, process identity, operation ID, timestamp, and operation kind. |
| Add `inspect_operation_lock` | Distinguishes active, stale, unknown, and conflicting lock states. |
| Add `repair_recovery` and `rollback_recovery` skeletons | Provide operation-specific repair entry points for switch, restore, apply incoming, discard, delete, and squash. |
| Add guarded `clear_stale_lock` | Clears abandoned locks only after diagnostics. |

**Acceptance:** a crash with recovery metadata and/or a stale lock returns actionable repair state instead of leaving the workspace permanently blocked. This is only partially met until repair/rollback execute operation-specific recovery.

## Slice 3: target-tree collision preflight

**Goal:** prevent checkout-like operations from overwriting local files that are not currently tracked business content.

**Current status:** Partially implemented and enabled in capabilities. The shared scanner reports ignored and policy-excluded target-path hazards for switch, restore, apply incoming, and apply shelf.

| Work item | Outcome |
|---|---|
| Add shared target-tree scanner | Detects ignored and policy-excluded hazards today; generated, distinct untracked hazard reporting, symlink, submodule, case, and Unicode hazards remain future expansion. |
| Wire scanner into switch, restore, apply incoming, and shelf apply preflights | Makes key existing file-writing operations use the same collision model. |
| Return structured `FileHazard` values | Lets hosts show precise blockers and safe next actions. |

**Acceptance:** switching, restoring, applying incoming changes, or applying shelves blocks before overwriting ignored or policy-excluded target-path files. Generated/platform-specific hazards and distinct untracked hazard reporting remain follow-up work.

## Slice 4: content-policy and Git metadata diagnostics

**Goal:** expose when Git ignore/attributes behavior can hide or transform content that the host policy says is business content.

**Current status:** Partially implemented. Current ignored-policy-file diagnostics and `audit_content_policy` exist; attributes, filters, normalization, and historical migration/redaction remain limited.

| Work item | Outcome |
|---|---|
| Add `audit_content_policy` | Finds current policy-tracked files hidden by ignore rules; historical out-of-policy reporting is reserved but not implemented. |
| Add lightweight `policy_git_diagnostics` | Provides current-workspace warnings for save, switch, restore, publish, and adoption preflight. |
| Add policy migration/redaction preflight shape | Defines the future path without silently rewriting history. |

**Acceptance:** hosts can warn that "everything is saved" may be false when policy-tracked files are ignored or transformed by Git metadata.

## Slice 5: adoption/setup preflight

**Goal:** make opening an existing repository a read-only setup decision, not an implicit promise that the repo already matches Draftline's model.

**Current status:** Partially implemented. `preflight_adopt_workspace`, sharing-mode diagnostics, and agent instruction text exist; broader repo-shape diagnostics and app-specific migration choices remain.

| Work item | Outcome |
|---|---|
| Add `preflight_adopt_workspace(policy)` | Maps branches, current HEAD, remotes, dirty state, policy fit, support refs, and unusual Git states. |
| Add sharing-mode diagnostics | Distinguishes local-only, local-with-remote, and cloned-from-remote workspaces. |
| Add agent instruction generation | Produces rules of engagement for coding agents operating in Draftline-managed repos. |

**Acceptance:** a host can open an arbitrary Git repo, explain blockers, and recommend safe setup paths without mutating refs, remotes, files, or policy state.

## Slice 6: publish leases and remote race safety

**Goal:** close the race between fetch and push.

**Current status:** Partially implemented. `preflight_publish` captures expected remote OID or absence, and tokenized `publish` rejects changed local state or changed remote-tracking state after its final fetch. The actual push still uses normal refspecs rather than explicit lease/create-only push mechanics.

| Work item | Outcome |
|---|---|
| Add `preflight_publish` | Captures expected remote ref OID or expected absence, plus remote identity. |
| Add token-bound `publish` | Fetches again and pushes only if the remote-tracking state still matches the preflight; explicit lease/create-only push refspecs remain future work. |
| Add explicit remote-race results | Distinguishes deleted, recreated, rewound, incoming, diverged, and expected-OID mismatch states. |

**Acceptance:** remote changes detected by the final fetch surface as business-shaped blockers. First publish create-only and normal publish lease semantics are not fully met until the push itself is lease/create-only protected.

## Slice 7: support-ref sync and archive recovery

**Goal:** make recovery points for shared work durable across machines without showing them as normal variations.

**Current status:** Partially implemented locally. Local support-ref listing, restoration, and expiration exist. General support-ref publish/fetch sync is not implemented; remote variation delete publishes one operation-specific support ref.

| Work item | Outcome |
|---|---|
| Add support-ref listing | Lists local support refs with source variation and target OID. Fetched remote support refs, actor/device, and age remain future work. |
| Add create-only support-ref publish | Not generally implemented; remote variation delete publishes one support ref before visible deletion. |
| Add support-ref fetch layout | Not implemented. |
| Add restore-as-variation | Implemented locally. |

**Acceptance:** delete/squash recovery points can be listed and restored locally without polluting normal variation views. Cross-machine publish/fetch remains future work.

## Slice 8: shared cleanup and history replacement

**Goal:** support team-visible delete and history replacement only when shared recovery is durable first.

**Current status:** Partially implemented for remote variation deletion. Shared history replacement is not implemented.

| Work item | Outcome |
|---|---|
| Add `preflight_delete_remote_variation` | Plans archive support ref, support-ref publish, and expected-OID remote deletion. |
| Add `delete_remote_variation` | Publishes support ref first, then deletes visible remote ref after fetch-and-compare. Explicit lease/create-only push mechanics remain future work. |
| Add `preflight_replace_remote_history` | Requires consent, replacement details, support-ref plan, and force-with-lease target. |
| Add `replace_remote_history` | Performs lease-protected history replacement only after support-ref publication succeeds. |

**Acceptance:** remote variation delete preserves the old tip in a support ref before visible deletion. Shared history replacement and explicit lease/create-only push mechanics remain future work.

## Slice 9: collaboration expansion

**Goal:** make collaboration complete beyond current-variation fast-forward.

**Current status:** Partially implemented. `remote_variations`, `adopt_remote_variation`, and `preflight_merge_incoming` exist. Prune/stale diagnostics and merge execution remain missing.

| Work item | Outcome |
|---|---|
| Add `remote_variations` and adoption | Lets users discover and adopt fetched teammate-created variations. |
| Add prune/stale remote diagnostics | Explains deleted or renamed remote variations without deleting local work automatically. |
| Add `preflight_merge_incoming` and `merge_incoming` | `preflight_merge_incoming` exists; `merge_incoming` execution remains future work. |

**Acceptance:** teammate-created and diverged variation discovery/preflight have explicit product flows. Deleted/renamed remote diagnostics and merge execution remain future work.

## Slice 10: shelf lifecycle

**Goal:** complete the "put work aside" promise.

**Current status:** Implemented for all-work shelves. Selected-file shelves and semantic conflict-resolution apply remain future work.

| Work item | Outcome |
|---|---|
| Add `shelve_changes` and `preflight_shelve_files` | `shelve_changes` supports all-work shelves; selected-file shelf preflight remains future work. |
| Add `list_shelves` and `preview_shelf` | Makes shelves discoverable and read-only previewable. |
| Add `preflight_apply_shelf` and `apply_shelf` | Applies shelves as file-writing operations with dirty-work and collision checks; conflict-resolution apply remains future work. |
| Add `delete_shelf` and optional `share_shelf` | `delete_shelf` exists; `share_shelf` remains future work if hosts want it. |

**Acceptance:** users can shelve all current work, list, preview, apply, and delete shelves without relying on hidden internal shelf refs.

## Slice 11: purge/redaction and retention

**Goal:** separate routine recovery retention from destructive sensitive-content removal.

**Current status:** Partially implemented. Local support-ref retention APIs, purge preflight, and purge verification exist. Destructive purge execution does not exist.

| Work item | Outcome |
|---|---|
| Add support-ref retention APIs | Expires local support refs as maintenance, not as a promise of sensitive-content deletion. Remote retention remains future work. |
| Add `preflight_purge_content` | Enumerates visible refs, support refs, tags, notes, replace refs, stash refs, remote-tracking refs, reflogs, alternates, and reachable objects. |
| Add `purge_content` and `verify_purge` | `verify_purge` exists; destructive `purge_content` remains future work. |

**Acceptance:** hosts can explain purge planning and distributed-Git limitations. Best-effort permanent removal is not implemented.

## Slice 12: agent and CLI facade

**Goal:** expose the same safety model to coding agents and automation.

**Current status:** Partially implemented for embedded Rust callers. JSON helpers and stable diagnostic explanations exist. No standalone CLI/tool facade exists, and `WorkspaceCapabilities::agent_cli_facade` reports `false`.

| Work item | Outcome |
|---|---|
| Add JSON result schema | Partially implemented through `inspect_json`, `capabilities_json`, diagnostics, safe next actions, retry class, and stable codes. |
| Add `draftline inspect`, `capabilities`, `preflight`, `execute`, and `verify` | Not implemented as CLI commands. |
| Add recovery commands | Not implemented as CLI commands; Rust recovery helpers exist. |
| Add `explain-error` | Implemented as a Rust helper. |

**Acceptance:** an embedded Rust agent can inspect, use operation-specific preflights, execute supported operations, verify workspace postconditions, and explain stable codes. CLI/tool agents still need a standalone facade.
