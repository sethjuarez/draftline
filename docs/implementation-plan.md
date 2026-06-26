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

**Current status:** Partially implemented. Lock metadata, `inspect_operation_lock`, and guarded `clear_stale_lock` exist. `repair_recovery` and `rollback_recovery` perform metadata-backed recovery for covered operations and return safe blockers when the ledger lacks enough state.

| Work item | Outcome |
|---|---|
| Add lock metadata | Record owner, process identity, operation ID, timestamp, and operation kind. |
| Add `inspect_operation_lock` | Distinguishes active, stale, unknown, and conflicting lock states. |
| Add `repair_recovery` and `rollback_recovery` | Repair or roll back operations when the ledger captured enough state; report safe blockers for operations that need richer recovery metadata. |
| Add guarded `clear_stale_lock` | Clears abandoned locks only after diagnostics. |

**Acceptance:** a crash with recovery metadata and/or a stale lock returns actionable repair state instead of leaving the workspace permanently blocked. This is partially met: covered operations can be repaired or rolled back, while operations without sufficient ledger metadata still return explicit blockers.

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

**Current status:** Implemented for current-variation publish. `preflight_publish` captures expected remote OID or absence, tokenized `publish` rejects changed local state or changed remote-tracking state after its final fetch, and push negotiation enforces expected remote old/new OIDs before upload.

| Work item | Outcome |
|---|---|
| Add `preflight_publish` | Captures expected remote ref OID or expected absence, plus remote identity. |
| Add token-bound `publish` | Fetches again and pushes only if the remote-tracking state still matches the preflight; push negotiation enforces expected remote old/new OIDs before upload. |
| Add explicit remote-race results | Distinguishes deleted, recreated, rewound, incoming, diverged, and expected-OID mismatch states. |

**Acceptance:** remote changes detected by the final fetch surface as business-shaped blockers, and first publish/create-only plus normal publish lease semantics are enforced during push negotiation before upload.

## Slice 7: support-ref sync and archive recovery

**Goal:** make recovery points for shared work durable across machines without showing them as normal variations.

**Current status:** Implemented for local publish/fetch/restore sync. Local and remote-tracking support-ref listing and restoration exist, and local expiration exists. Support refs can be published with create-only remote updates and fetched into a remote-tracking support-ref layout; remote variation delete and shared history replacement publish support refs before visible remote mutation.

| Work item | Outcome |
|---|---|
| Add support-ref listing | Lists local and remote-tracking support refs with source variation and target OID. Actor/device and age remain future work. |
| Add create-only support-ref publish | Implemented generally through `preflight_publish_support_refs` and `publish_support_refs`; remote variation delete also publishes one support ref before visible deletion. |
| Add support-ref fetch layout | Implemented with `fetch_support_refs`, fetching into `refs/remotes/<remote>/draftline/...` without overwriting local support refs. |
| Add restore-as-variation | Implemented for local and remote-tracking support refs through `preflight_restore_support_ref`, `restore_support_ref`, and compatibility `restore_support_ref_as_variation`. |

**Acceptance:** delete/squash recovery points can be listed and restored without polluting normal variation views, and they can be published/fetched across machines into hidden remote-tracking support refs. Remote retention remains future work.

## Slice 8: shared cleanup and history replacement

**Goal:** support team-visible delete and history replacement only when shared recovery is durable first.

**Current status:** Implemented for remote variation deletion and current-variation shared history replacement. Replacement tokens require explicit confirmation before execution. Broader admin UX and multi-variation replacement policy remain host/future work.

| Work item | Outcome |
|---|---|
| Add `preflight_delete_remote_variation` | Plans archive support ref, support-ref publish, and expected-OID remote deletion. |
| Add `delete_remote_variation` | Publishes support ref first with create-only negotiation, then deletes the visible remote ref with expected-OID negotiation after fetch-and-compare. |
| Add `preflight_replace_remote_history` | Implemented for the current variation with replacement details, support-ref plan, and force-with-lease target. |
| Add `replace_remote_history` | Implemented: requires explicit token confirmation, publishes support refs first, then performs lease-protected history replacement. |

**Acceptance:** remote variation delete and current-variation shared history replacement preserve recovery support refs before visible remote mutation and guard remote pushes with negotiated expectations; shared history replacement also requires explicit token confirmation.

## Slice 9: collaboration expansion

**Goal:** make collaboration complete beyond current-variation fast-forward.

**Current status:** Partially implemented. `fetch_all_variations`, `remote_variations`, `remote_variation_diagnostics`, `adopt_remote_variation`, `preflight_merge_incoming`, and clean `merge_incoming` execution exist. Rename inference, tokenized adoption, and user-driven conflict resolution remain missing.

| Work item | Outcome |
|---|---|
| Add `remote_variations` and adoption | Lets users discover and adopt fetched teammate-created variations. |
| Add prune/stale remote diagnostics | Implemented with fetch-all/prune plus local-only and remote-only variation diagnostics. |
| Add `preflight_merge_incoming` and `merge_incoming` | Clean semantic merge execution exists; unresolved conflict resolution remains future work. |

**Acceptance:** teammate-created/deleted and diverged variation discovery/preflight have explicit product flows, and clean diverged merges can be written as two-parent versions. Rename inference and conflict-resolution merge execution remain future work.

## Slice 10: shelf lifecycle

**Goal:** complete the "put work aside" promise.

**Current status:** Implemented for all-work and selected-file shelves. Semantic conflict-resolution apply remains future work.

| Work item | Outcome |
|---|---|
| Add `shelve_changes` and `preflight_shelve_files` | `shelve_changes` supports all-work shelves; `preflight_shelve_files` and `shelve_files` support selected-file shelves. |
| Add `list_shelves` and `preview_shelf` | Makes shelves discoverable and read-only previewable. |
| Add `preflight_apply_shelf` and `apply_shelf` | Applies shelves as file-writing operations with dirty-work and collision checks; conflict-resolution apply remains future work. |
| Add `delete_shelf` and optional `share_shelf` | `delete_shelf` exists; `share_shelf` remains future work if hosts want it. |

**Acceptance:** users can shelve all current work or selected files, list, preview, apply, and delete shelves without relying on hidden internal shelf refs.

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

**Current status:** Partially implemented for embedded Rust callers and a minimal standalone CLI. JSON helpers, stable diagnostic explanations, and `draftline inspect --json`, `capabilities --json`, `verify --json`, and `explain-error --json` exist; generic CLI preflight/execute and recovery commands remain future work.

| Work item | Outcome |
|---|---|
| Add JSON result schema | Partially implemented through `inspect_json`, `capabilities_json`, diagnostics, safe next actions, retry class, and stable codes. |
| Add `draftline inspect`, `capabilities`, `preflight`, `execute`, and `verify` | `inspect`, `capabilities`, and `verify` are implemented as JSON CLI commands; generic `preflight` and `execute` remain future work. |
| Add recovery commands | Not implemented as CLI commands; Rust recovery helpers exist. |
| Add `explain-error` | Implemented as a Rust helper and JSON CLI command. |

**Acceptance:** an embedded Rust agent can inspect, use operation-specific preflights, execute supported operations, verify workspace postconditions, and explain stable codes. CLI agents can inspect, verify, list capabilities, and explain stable codes; generic CLI mutation remains future work.
