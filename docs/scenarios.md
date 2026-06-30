# Draftline scenarios

This document is the entry point for Draftline's product scenario contract. The detailed flows now live in focused files so each area can evolve without turning one document into a scroll-only artifact.

Draftline scenarios start from user intent, then call out why the scenario exists, how Draftline should execute it, which invariant protects the user, and whether the current crate covers the scenario.

See the [Draftline API plan](api-plan.md) for the roadmap from these scenarios to Rust APIs, CLI commands, and agent/tool surfaces. See the [implementation plan](implementation-plan.md) for the proposed engineering sequence.

## Scenario documents

| Document | Covers |
|---|---|
| [Workspace and agent setup](scenario-flows/workspace.md) | Start/open/adopt flows, sharing mode, remote bootstrap, and agent rules of engagement. |
| [Content policy](scenario-flows/content-policy.md) | Business-content boundaries, policy changes, and Git ignore/attributes hazards. |
| [Authoring and versions](scenario-flows/authoring.md) | Current state, saves, discard, variations, switching, preview, and restore. |
| [Collaboration](scenario-flows/collaboration.md) | Publish, apply incoming, remote variation lifecycle, remote destination changes, and merge. |
| [Recovery and cleanup](scenario-flows/recovery-cleanup.md) | Shelves, delete/squash, support refs, purge/redaction, binary assets, interruption, out-of-band mutation, and stale locks. |
| [Coverage and roadmap](coverage.md) | Executive coverage, primitive coverage, and follow-up priority gaps. |
| [Product language](product-language.md) | Mapping between product actions and Git-backed implementation. |

## Coverage legend

| Status | Meaning |
|---|---|
| Covered | Existing primitives support the scenario safely. |
| Covered for `<scope>` | Existing primitives support only the named scope. Anything outside that phrase is not covered unless another row says so. |
| Partially covered | The safe foundation exists, but the full business workflow still needs one or more primitives or UI steps. |
| Planning-only | Draftline exposes inspection, preflight, or verification shape, but does not execute the user-visible mutation. |
| Not covered | The scenario is identified, but Draftline does not yet expose the needed primitive. |
| Host concern | Draftline exposes the low-level signal; the embedding app owns product copy, UX, auth, or policy decisions. |

Use the narrowest truthful status. Prefer "Covered for all-work shelves" over "Covered" when selected-file shelves, sharing, or conflict-resolution apply are still outside the implementation.

## Doc-to-test coverage matrix

Every documented flow must either name executable coverage or explain why the remaining business scenario is intentionally unsupported. Scenario tests live in `crates/draftline/tests/scenario_flows.rs`; lower-level Rust tests live in `crates/draftline/src/workspace.rs`; Tauri/client contract tests live under `crates/draftline/tests`.

| Flow | Status | Executable evidence | Unsupported or future coverage |
|---|---|---|---|
| Flow 1: start or open a workspace | Covered | `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | None for MVP. |
| Flow 1a: adopt an existing non-Draftline repo | Partially covered | `scenario_flow_2b_content_policy_api_surfaces_ignored_tracked_content`, `tauri_contract_keeps_frontend_json_shape_stable` | Host-owned migration decisions, branch/remote mapping, actor identity, and deeper Git attributes/filter diagnostics remain future work. |
| Flow 1b: choose or discover sharing mode | Partially covered | `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `tauri_contract_smokes_publish_current_variation` | Support-ref bootstrap policy and remote destination confirmation are host/product work. |
| Flow 1c: add a remote after local work exists | Partially covered | `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `tauri_contract_smokes_publish_current_variation` | First publish is tokenized and executable; support-ref bootstrap and destination confirmation copy remain future coverage. |
| Flow 1d: start from a shared remote | Covered for clone/open and fetched remote variations | `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `remote_variations_can_be_discovered_and_adopted_locally`, `support_refs_publish_create_only_and_fetch_remote_tracking_refs` | Product diagnostics after clone/fetch-all and automatic support-ref discovery remain future UX work. |
| Flow 1e: developer Copilot opens a Draftline-managed repo | Partially covered | `tauri_contract_keeps_frontend_json_shape_stable`, `clear_stale_lock_removes_only_stale_metadata_lock` | Generated repository instruction files and standalone CLI/tool commands are not implemented. |
| Flow 1f: agent uses Draftline APIs directly | Partially covered | `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_publish_current_variation`, `tauri_contract_smokes_collaboration_incoming_and_merge` | Generic `draftline preflight/execute/verify/recovery` CLI or tool facade remains future work. |
| Flow 2: configure what counts as business content | Covered | `scenario_flow_2b_content_policy_api_surfaces_ignored_tracked_content`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | Host apps still own their product-specific policy choices. |
| Flow 2a: change content policy after work exists | Not covered | `scenario_flow_2b_content_policy_api_surfaces_ignored_tracked_content` proves current diagnostics only | Historical policy migration/redaction is intentionally unsupported until an explicit audit/migrate/redact model exists. |
| Flow 2b: content policy conflicts with Git ignore or attributes | Partially covered | `scenario_flow_2b_content_policy_api_surfaces_ignored_tracked_content` | Ignored-file diagnostics exist; Git attributes, filters, path normalization, and historical policy diagnostics remain future work. |
| Flow 3: understand current state | Covered | `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | Better product copy for unusual Git states remains future work. |
| Flow 4: save business work | Covered | `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | No behavior gap for standard save. |
| Flow 4a: save or shelve selected work | Covered for selected files | `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | Selected-file conflict UX and any policy for shared shelves remain future work. |
| Flow 5: abandon unsaved edits | Covered | `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | Switch-time discard remains intentionally unsupported; use the explicit discard flow first. |
| Flow 6: try another direction | Covered | `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle`, `tauri_contract_preflights_remote_aware_variation_creation` | None for MVP. |
| Flow 6a: rename or relabel a direction | Covered for display metadata | `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle`, `tauri_contract_rejects_stale_rename_token` | True Git ref rename is intentionally unsupported and would need separate archive-first semantics. |
| Flow 7: move between directions | Covered for full-variation switching | `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle` | Selected-file switch/shelve remains future work. |
| Flow 8: review older work | Covered | `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | None for MVP. |
| Flow 9: restore older work | Partially covered | `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle`, `targeted_restore_creates_save_on_existing_variation_without_wrong_branch_write`, `tauri_contract_restores_version_to_target_variation` | Richer restore preflight for old-policy content remains future work. |
| Flow 9a: target tree collides with local non-versioned files | Partially covered | `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle`, `tauri_contract_smokes_collaboration_incoming_and_merge` | Symlink, submodule, case, Unicode, generated-file, and richer untracked hazard reporting remain future work. |
| Flow 10: publish my work to the team | Covered for current variation | `scenario_flow_10_11_12_collaboration_fast_forward_and_clean_merge`, `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `tauri_contract_smokes_publish_current_variation` | Broader product result copy for branch disappearance/recreation remains future work. |
| Flow 11: receive teammate updates | Covered for current-variation fast-forward only | `scenario_flow_10_11_12_collaboration_fast_forward_and_clean_merge`, `tauri_contract_smokes_collaboration_incoming_and_merge` | Diverged conflict UX is Flow 12; broad remote lifecycle diagnostics remain separate. |
| Flow 11a: discover teammate-created directions | Partially covered | `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `remote_variations_can_be_discovered_and_adopted_locally` | Tokenized adoption and richer product copy remain future work. |
| Flow 11b: remote variation was deleted or renamed | Partially covered | `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `remote_variation_diagnostics_reports_local_and_remote_only_variations_after_prune` | Rename inference and higher-level host messaging remain future work. |
| Flow 11c: change or remove remote destination | Partially covered | `tauri_contract_smokes_publish_current_variation` | Updating an existing remote URL is still silent; preflight/confirmation/remove APIs are not implemented. |
| Flow 12: reconcile teammate changes | Covered for clean semantic merges | `scenario_flow_10_11_12_collaboration_fast_forward_and_clean_merge`, `scenario_flow_12_conflict_preflight_reports_without_mutating`, `tauri_contract_smokes_collaboration_incoming_and_merge` | User-driven execution for unresolved conflicts remains future work. |
| Flow 12a: apply shelved work | Covered for clean all-work shelves | `scenario_flow_12a_shelf_apply_preview_and_delete_roundtrip`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` | Selected-file shelves and conflict-resolution apply remain future work. |
| Flow 13: remove or clean up work | Covered for local delete, local squash, and local milestone compaction | `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle`, `scenario_flow_13a_local_milestone_compaction_preview_apply_resolve_and_undo` | Product copy for candidate selection and unsupported ranges remains future work. |
| Flow 13a: compact local version history | Covered for linear milestone compaction | `scenario_flow_13a_local_milestone_compaction_preview_apply_resolve_and_undo`, `history_cleanup_compacts_milestones_maps_old_versions_and_undoes`, `history_cleanup_compacts_middle_range_and_replays_descendants` | Multi-range and richer semantic conflict handling remain future work. |
| Flow 13b: remove or rewrite shared work | Covered for shared variation delete, current-variation replacement, and published compaction | `replace_remote_history_publishes_support_ref_before_force_with_lease`, `delete_remote_variation_publishes_support_ref_before_deleting_visible_ref`, `history_cleanup_publish_replaces_remote_with_support_ref_and_lease`, `history_cleanup_publish_support_ref_targets_remote_tip_when_local_was_ahead` | Protected-branch/server-capability diagnostics and teammate-facing copy remain future work. |
| Flow 13c: sync hidden recovery support refs | Covered for local publish/fetch/restore | `scenario_flow_13c_13d_remote_support_refs_roundtrip_restore_and_local_expire`, `support_refs_publish_create_only_and_fetch_remote_tracking_refs`, `support_ref_publish_preflight_rejects_same_name_different_oid_collision` | Remote retention is not implemented. |
| Flow 13d: recover cleanup after clone or device loss | Covered for fetched support refs | `scenario_flow_13c_13d_remote_support_refs_roundtrip_restore_and_local_expire`, `support_refs_publish_create_only_and_fetch_remote_tracking_refs` | Remote retention/expiration policy remains future work. |
| Flow 13e: sync incoming compacted remote history | Covered for safe incoming rewrites | `scenario_flow_13e_remote_compaction_publish_sync_replay_and_dirty_block`, `apply_incoming_accepts_published_remote_compaction_when_clean`, `apply_incoming_replays_local_snapshots_after_published_remote_compaction`, `remote_compaction_with_non_first_parent_local_work_stays_needs_merge` | Host copy for dirty-work choices and "incoming compaction" remains future work. |
| Flow 13f: permanently purge or redact content | Planning-only | `scenario_flow_13f_purge_api_is_explicitly_planning_only` | Destructive purge execution is intentionally not implemented because distributed Git cannot guarantee deletion from existing clones. |
| Flow 13g: expire old support refs | Partially covered locally | `scenario_flow_13c_13d_remote_support_refs_roundtrip_restore_and_local_expire` | Remote support-ref retention with permissions, audit, and GC guidance remains future work. |
| Flow 13h: large or binary business assets | Partially covered | `tauri_contract_keeps_frontend_json_shape_stable` | Detection exists; block/warn/stream/external storage policy is host/future work. |
| Flow 14: recover from interruption or unusual state | Partially covered | `delete_remote_variation_retries_after_support_ref_was_already_published`, `repair_remote_delete_recovers_after_visible_ref_was_deleted`, `clear_stale_lock_removes_only_stale_metadata_lock` | Recovery repair is operation-specific; broader repair/rollback metadata is still expanding. |
| Flow 14a: out-of-band Git mutation | Partially covered | `tauri_contract_keeps_frontend_json_shape_stable`, `clear_stale_lock_removes_only_stale_metadata_lock` | Deeper repair for detached HEAD, raw branch changes, conflict indexes, and non-Draftline rewrites remains future work. |
| Flow 14b: stale or abandoned operation lock | Partially covered | `clear_stale_lock_removes_only_stale_metadata_lock` | Integrated lock-plus-recovery repair remains future work. |

## Business safety principles

1. Users should choose product actions, not Git commands.
2. "Look around" flows must be read-only.
3. "Keep this" flows should create a named version, variation, shelf, or archive ref.
4. "Move to something else" flows must preflight local unsaved work.
5. "Share or receive team work" flows must fetch latest remote state before deciding.
6. "We both changed it" flows require explicit merge or conflict resolution.
7. "Go back" creates a new save; it must not reset history.
8. "Abandon edits" must be explicit and content-policy-aware.
9. "Clean up" and "remove" flows must preserve old tips under `refs/draftline/...`, unless an explicit purge/redaction operation overrides recovery.
10. Interrupted operations should produce a recovery prompt before normal work resumes.
11. Content policy changes are not retroactive unless an explicit migration or redaction operation says so.
12. Archive retention and permanent deletion are separate business intents.
13. Remote state includes remote identity and branch existence, not only ahead/behind counts.
14. Every mutating operation should state whether it affects all tracked changes or a selected subset.
15. The shared remote is the trust boundary for shared work; Draftline recovery support refs are hidden from normal views, not private from collaborators.
16. Shared recovery requires explicit support-ref sync; local archive refs alone are not a cross-machine guarantee.
17. Publishing support refs must be append-only: never force-overwrite a recovery point.
18. Shelves are personal work-in-progress by default; sharing shelved work requires a separate explicit policy.
19. Workspaces have a sharing mode: local-only, local with a remote added later, or cloned from a remote. Flows must not assume `origin` exists.
20. Any operation that writes a target tree into the workspace must preflight collisions against tracked, untracked, ignored, and current-policy-excluded files.
21. Remote mutations must use expected remote identity, not just "fetch then decide"; branch deletion, recreation, or rewind after fetch is a first-class race.

## Principled support model

Every supported scenario should answer the same questions:

| Question | Purpose |
|---|---|
| What is the user trying to do? | Keeps the API anchored in business intent instead of Git vocabulary. |
| Why does this scenario exist? | Identifies the risk, collaboration need, or product promise behind the flow. |
| How does Draftline execute it? | Names the exact primitive sequence and which operations are read-only or mutating. |
| What invariant protects the user? | States the rule that prevents data loss, hidden overwrites, or confusing detached states. |
| What should the host show? | Separates library behavior from app-owned confirmation, copy, and recovery UX. |
| What is missing? | Makes partial coverage explicit instead of implying safety by omission. |

```mermaid
flowchart LR
    A[Business intent] --> B[Classify scenario]
    B --> C{Read-only?}
    C -- Yes --> D[Use preview, diff, summary, status]
    C -- No --> E{Can lose or hide work?}
    E -- Yes --> F[Preflight plus explicit user choice]
    E -- No --> G[Append-only or ref-creating primitive]
    F --> H[Mutate with operation lock and recovery state]
    G --> H
    H --> I{Ref deleted or rewritten?}
    I -- Yes --> J[Archive old tip under refs/draftline]
    I -- No --> K[Complete operation]
    J --> K
    K --> L[Return business-shaped result]
```

## Design rules by user intent

| User intent | Product action | Git-backed shape | Why this shape |
|---|---|---|---|
| Look around | Summary, preview, diff, status | Read trees, diffs, refs, and status only | Users should be able to inspect history without changing files. |
| Keep current work | Save version | Commit tracked content | A named save is the durable unit users understand. |
| Keep selected work | Save selected files | Not yet exposed | Mixed ready/unfinished edits need partial workflows. |
| Try another idea | Create variation | Create branch/ref | Alternatives need stable names without detached HEAD. |
| Rename an idea | Edit variation metadata | Config metadata, not ref rename | Product labels should change without rewriting Git branch identity. |
| Move to another idea | Switch variation | Preflight, optional save/shelve, checkout branch | Switching writes files, so dirty work needs an explicit plan first. |
| Put work aside | Shelve | Local support ref by default | Temporary work should be recoverable without silently publishing unfinished work. |
| Abandon edits | Discard | Policy-aware checkout/reset/removal | Destructive local cleanup must be explicit and scoped to tracked content. |
| Share work | Publish | Fetch, check ahead/behind, push current variation | Publishing must not overwrite teammate work. |
| Receive work | Apply incoming | Fetch, preflight, fast-forward only | Remote updates are safe only when local history can advance without merge. |
| Reconcile work | Merge incoming | Three-way merge with semantic conflict model | Divergence needs human-readable conflict decisions, not hidden Git merges. |
| Go back | Restore as new save | New commit from old tree | Restoring should preserve the audit trail instead of moving history backward. |
| Clean up | Compact, squash, or delete | Archive old tip as a support ref, then rewrite/delete visible ref | Cleanup should simplify UI while preserving a hidden recovery pointer and stale-version mapping. |
| Recover | Recovery prompt | Read ledger, block normal operations, repair/rollback/acknowledge | Interrupted operations should be visible and deliberate. |
| Permanently remove content | Purge/redact | Not yet exposed | True deletion conflicts with archive-first safety and needs a separate explicit workflow. |

## Visible work vs support refs

Draftline should distinguish normal business views from the hidden support refs that make recovery possible.

| Ref namespace | Product meaning | Normal UI visibility | Sync policy |
|---|---|---|---|
| `refs/heads/<variation>` | Visible team variations | Shown in normal variation/history views | Published/fetched as shared work. |
| `refs/draftline/shelves/...` | Work intentionally set aside | Hidden from normal views; shown in shelf/recovery views | Local-only by default; any sharing must be explicit and separately permissioned. |
| `refs/draftline/deleted-variations/...` | Recovery points for deleted variations | Hidden from normal views; shown in recovery/admin views | Published/fetched explicitly as shared support refs when the user shares cleanup recovery. |
| `refs/draftline/rewrites/...` | Recovery points for history rewrites such as squash or compaction | Hidden from normal views; shown in recovery/admin views | Published before shared history replacement and fetched with remote sync so incoming compaction can be recognized safely. |

Shared recovery support refs are **not private**. They live inside the same shared repository trust boundary as visible work. They are hidden because they are not primary business objects, not because they contain secret content. Git server ACLs and refspec policy, not naming, determine who can fetch them. If content must not be retained by collaborators, the right scenario is purge/redaction, not delete, squash, or shared recovery.

Shelves are different from cleanup archives. A shelf may contain unfinished or sensitive local work that the user never intended to publish, so shelf sync must be local-only by default or require an explicit "share shelved work" flow with clear warning copy and permissions.

Support-ref sync needs its own ref contract:

| Rule | Requirement |
|---|---|
| Unique names | Archive refs should include operation identity and source context, such as source ref, old tip OID, operation UUID, actor/device, and time. |
| Immutability | Once published, a recovery support ref must not be force-updated or reused for another old tip. |
| Race safety | Shared cleanup must push support refs with "create only" semantics and delete/rewrite visible refs only with expected-OID or lease checks. |
| Local/remote mapping | Fetched remote support refs should not overwrite unsynced local support refs; use a remote-tracking layout such as `refs/draftline/remotes/<remote>/...` or an equivalent indexed model. |
| Host permissions | Some Git servers may reject non-branch namespaces. Support-ref sync must surface that as a host/remote capability issue, not silently fall back to unsafe cleanup. |

## Scenario support checklist

Before adding or changing a primitive, verify the scenario can answer:

1. Is the operation read-only, append-only, ref-creating, file-writing, ref-moving, or ref-deleting?
2. If it writes files, does it have a preflight report or an explicit policy?
3. If it can overwrite unsaved work, does it require save, shelve, discard, or cancel?
4. If it talks to a remote, does it fetch before deciding and refuse unsafe divergence?
5. If it deletes or rewrites a ref, does it preserve the old tip under `refs/draftline/...`?
6. If it can be interrupted, does it write recovery state before mutation and clear it after completion?
7. If it returns an ID, does the ID round-trip without parsing business semantics?
8. If a path is provided, is it normalized, workspace-relative, and content-policy-aware where needed?
9. If it creates support refs, does the scenario state whether those refs are local-only today or synced to the shared remote?
10. If it syncs support refs, are naming, immutability, refspec mapping, permissions, and failure ordering defined?
11. If the scenario is only partially covered, is the missing primitive listed in the roadmap?

## Full business lifecycle

```mermaid
flowchart TD
    A[Start workspace] --> B[Configure content boundaries]
    B --> C[Understand current state]
    C --> D{User intent}

    D -- Create or edit --> E[Authoring scenarios]
    D -- Abandon edits --> F[Discard scenarios]
    D -- Try another direction --> G[Variation scenarios]
    D -- Review history --> H[Version and history scenarios]
    D -- Restore old work --> I[Restore scenarios]
    D -- Work with teammates --> J[Remote and collaboration scenarios]
    D -- Clean up --> K[Cleanup scenarios]
    D -- Something looks wrong --> L[Recovery and error scenarios]

    E --> C
    F --> C
    G --> C
    H --> C
    I --> C
    J --> C
    K --> C
    L --> C
```
