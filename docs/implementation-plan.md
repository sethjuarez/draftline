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

| Work item | Outcome |
|---|---|
| Add `Workspace::inspect()` | Reports workspace ID, sharing mode, current variation, dirty state, remotes, content-policy diagnostics, recovery, lock state, support refs, and safe next actions. |
| Add `Workspace::capabilities()` | Reports which Draftline features exist in this crate version. |
| Introduce `SafeNextAction` and stable diagnostic codes | Lets apps and agents react without parsing prose. |

**Acceptance:** callers can determine whether normal work, recovery, remote setup, or user choice is required without running a mutating operation.

## Slice 2: recovery and stale-lock repair

**Goal:** make interrupted operations repairable instead of only acknowledgeable.

| Work item | Outcome |
|---|---|
| Add lock metadata | Record owner, process identity, operation ID, timestamp, and operation kind. |
| Add `inspect_operation_lock` | Distinguishes active, stale, unknown, and conflicting lock states. |
| Add `repair_recovery` and `rollback_recovery` skeletons | Provide operation-specific repair entry points for switch, restore, apply incoming, discard, delete, and squash. |
| Add guarded `clear_stale_lock` | Clears abandoned locks only after diagnostics. |

**Acceptance:** a crash with recovery metadata and/or a stale lock returns actionable repair state instead of leaving the workspace permanently blocked.

## Slice 3: target-tree collision preflight

**Goal:** prevent checkout-like operations from overwriting local files that are not currently tracked business content.

| Work item | Outcome |
|---|---|
| Add shared target-tree scanner | Detects untracked, ignored, generated, policy-excluded, symlink, submodule, case, and Unicode path hazards. |
| Wire scanner into switch, restore, and apply incoming preflights | Makes all existing file-writing operations use the same collision model. |
| Return structured `FileHazard` values | Lets hosts show precise blockers and safe next actions. |

**Acceptance:** switching, restoring, or applying incoming changes blocks before overwriting untracked, ignored, generated, or policy-excluded local files.

## Slice 4: content-policy and Git metadata diagnostics

**Goal:** expose when Git ignore/attributes behavior can hide or transform content that the host policy says is business content.

| Work item | Outcome |
|---|---|
| Add `audit_content_policy` | Finds policy-tracked files hidden by ignore rules and history content outside current policy. |
| Add lightweight `policy_git_diagnostics` | Provides current-workspace warnings for save, switch, restore, publish, and adoption preflight. |
| Add policy migration/redaction preflight shape | Defines the future path without silently rewriting history. |

**Acceptance:** hosts can warn that "everything is saved" may be false when policy-tracked files are ignored or transformed by Git metadata.

## Slice 5: adoption/setup preflight

**Goal:** make opening an existing repository a read-only setup decision, not an implicit promise that the repo already matches Draftline's model.

| Work item | Outcome |
|---|---|
| Add `preflight_adopt_workspace(policy)` | Maps branches, current HEAD, remotes, dirty state, policy fit, support refs, and unusual Git states. |
| Add sharing-mode diagnostics | Distinguishes local-only, local-with-remote, and cloned-from-remote workspaces. |
| Add agent instruction generation | Produces rules of engagement for coding agents operating in Draftline-managed repos. |

**Acceptance:** a host can open an arbitrary Git repo, explain blockers, and recommend safe setup paths without mutating refs, remotes, files, or policy state.

## Slice 6: publish leases and remote race safety

**Goal:** close the race between fetch and push.

| Work item | Outcome |
|---|---|
| Add `preflight_publish` | Captures expected remote ref OID or expected absence, plus remote identity. |
| Add lease-bound `publish` | Pushes only if the remote still matches the preflight. |
| Add explicit remote-race results | Distinguishes deleted, recreated, rewound, incoming, diverged, and expected-OID mismatch states. |

**Acceptance:** first publish is create-only, normal publish is expected-OID protected, and remote changes after preflight surface as business-shaped blockers.

## Slice 7: support-ref sync and archive recovery

**Goal:** make recovery points for shared work durable across machines without showing them as normal variations.

| Work item | Outcome |
|---|---|
| Add support-ref listing | Lists local and fetched support refs with source operation, old tip, source ref, actor/device, and age. |
| Add create-only support-ref publish | Publishes support refs append-only and refuses overwrites. |
| Add support-ref fetch layout | Fetches remote support refs into a non-overwriting remote-tracking namespace. |
| Add restore-as-variation | Restores an archive ref as a new visible variation with a non-conflicting name. |

**Acceptance:** delete/squash recovery points can be published, fetched elsewhere, listed, and restored without polluting normal variation views.

## Slice 8: shared cleanup and history replacement

**Goal:** support team-visible delete and history replacement only when shared recovery is durable first.

| Work item | Outcome |
|---|---|
| Add `preflight_delete_remote_variation` | Plans archive support ref, support-ref publish, and expected-OID remote deletion. |
| Add `delete_remote_variation` | Publishes support ref first, then deletes visible remote ref by lease. |
| Add `preflight_replace_remote_history` | Requires consent, replacement details, support-ref plan, and force-with-lease target. |
| Add `replace_remote_history` | Performs lease-protected history replacement only after support-ref publication succeeds. |

**Acceptance:** shared visible refs are never deleted or replaced unless the old tip is durably preserved in the shared support-ref namespace.

## Slice 9: collaboration expansion

**Goal:** make collaboration complete beyond current-variation fast-forward.

| Work item | Outcome |
|---|---|
| Add `remote_variations` and adoption preflight | Lets users discover and adopt teammate-created variations. |
| Add prune/stale remote diagnostics | Explains deleted or renamed remote variations without deleting local work automatically. |
| Add `preflight_merge_incoming` and `merge_incoming` | Turns `NeedsMerge` into a safe conflict-resolution workflow backed by semantic resolvers. |

**Acceptance:** teammate-created, deleted, renamed, and diverged variations have explicit product flows instead of raw Git outcomes.

## Slice 10: shelf lifecycle

**Goal:** complete the "put work aside" promise.

| Work item | Outcome |
|---|---|
| Add `shelve_changes` and `preflight_shelve_files` | Supports all-work and selected-file shelves. |
| Add `list_shelves` and `preview_shelf` | Makes shelves discoverable and read-only previewable. |
| Add `preflight_apply_shelf` and `apply_shelf` | Applies shelves as merge-like file-writing operations with conflicts and collision checks. |
| Add `delete_shelf` and optional `share_shelf` | Keeps shelf deletion and sharing explicit. |

**Acceptance:** users can shelve, list, preview, apply, and delete work without relying on hidden internal shelf refs.

## Slice 11: purge/redaction and retention

**Goal:** separate routine recovery retention from destructive sensitive-content removal.

| Work item | Outcome |
|---|---|
| Add support-ref retention APIs | Expires support refs as maintenance, not as a promise of sensitive-content deletion. |
| Add `preflight_purge_content` | Enumerates visible refs, support refs, tags, notes, replace refs, stash refs, remote-tracking refs, reflogs, alternates, and reachable objects. |
| Add `purge_content` and `verify_purge` | Executes an explicit admin destructive workflow with postcondition checks and clear distributed-Git limitations. |

**Acceptance:** hosts can explain the difference between recoverability cleanup and best-effort permanent removal.

## Slice 12: agent and CLI facade

**Goal:** expose the same safety model to coding agents and automation.

| Work item | Outcome |
|---|---|
| Add JSON result schema | Preserves blockers, warnings, safe next actions, retry class, and stable codes. |
| Add `draftline inspect`, `capabilities`, `preflight`, `execute`, and `verify` | Gives agents a narrow typed protocol instead of raw Git. |
| Add recovery commands | Exposes diagnose, repair, rollback, and clear-stale-lock flows. |
| Add `explain-error` | Maps stable codes to safe next actions and host/user copy. |

**Acceptance:** an agent can inspect, preflight, execute tokenized operations, verify postconditions, and recover from failures without parsing Git output or mutating Draftline-owned state directly.
