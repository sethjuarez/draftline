# Draftline coverage and roadmap

[Back to scenario index](scenarios.md)

## Executive scenario coverage

Status values are intentionally narrow. `Covered for <scope>` means only that named scope is implemented; the `Remaining gap` column is still out of scope for current consumers.

The canonical per-flow doc-to-test matrix is in [Draftline scenarios](scenarios.md#doc-to-test-coverage-matrix). This page summarizes release-gate evidence by scenario area; if a documented flow has no executable evidence, the matrix must name the unsupported reason and intended future coverage.

## Release-gate scenario evidence

| Coverage area | Documented flows | Primary executable evidence |
|---|---|---|
| Remote bootstrap, first publish, remote-only discovery, and prune diagnostics | Flow 1c, Flow 11a, Flow 11b | `scenario_flow_1c_11a_11b_remote_bootstrap_variation_diagnostics_and_adoption`, `remote_variations_can_be_discovered_and_adopted_locally`, `remote_variation_diagnostics_reports_local_and_remote_only_variations_after_prune` |
| Current-variation publish, incoming fast-forward, and clean merge | Flow 10, Flow 11, Flow 12 | `scenario_flow_10_11_12_collaboration_fast_forward_and_clean_merge`, `scenario_flow_12_conflict_preflight_reports_without_mutating`, `tauri_contract_smokes_publish_current_variation`, `tauri_contract_smokes_collaboration_incoming_and_merge` |
| Local cleanup, milestone compaction, stale-version resolution, and undo | Flow 13, Flow 13a | `scenario_flow_13a_local_milestone_compaction_preview_apply_resolve_and_undo`, `history_cleanup_compacts_milestones_maps_old_versions_and_undoes`, `history_cleanup_compacts_middle_range_and_replays_descendants` |
| Shared cleanup and support-ref-first remote rewrite/delete | Flow 13b | `replace_remote_history_publishes_support_ref_before_force_with_lease`, `delete_remote_variation_publishes_support_ref_before_deleting_visible_ref`, `history_cleanup_publish_replaces_remote_with_support_ref_and_lease`, `history_cleanup_publish_refuses_remote_race_after_preflight` |
| Support-ref sync, restore after clone/device loss, and local expiration | Flow 13c, Flow 13d, Flow 13g | `scenario_flow_13c_13d_remote_support_refs_roundtrip_restore_and_local_expire`, `support_refs_publish_create_only_and_fetch_remote_tracking_refs`, `support_ref_publish_preflight_rejects_same_name_different_oid_collision` |
| Incoming compacted remote history, dirty blocking, local-ahead replay, and ambiguous merge fallback | Flow 13e | `scenario_flow_13e_remote_compaction_publish_sync_replay_and_dirty_block`, `apply_incoming_accepts_published_remote_compaction_when_clean`, `apply_incoming_replays_local_snapshots_after_published_remote_compaction`, `remote_compaction_with_non_first_parent_local_work_stays_needs_merge` |
| Content policy, selected work, dirty workspace policy, and target-tree hazards | Flow 2, Flow 2b, Flow 4a, Flow 5, Flow 7, Flow 9a | `scenario_flow_2b_content_policy_api_surfaces_ignored_tracked_content`, `scenario_flow_6_7_9_13_local_variation_restore_and_support_ref_lifecycle`, `tauri_contract_keeps_frontend_json_shape_stable`, `tauri_contract_smokes_history_preview_restore_shelf_and_policy_commands` |
| Purge planning and explicit non-execution | Flow 13f | `scenario_flow_13f_purge_api_is_explicitly_planning_only` |
| Recovery, remote-delete repair, and stale locks | Flow 14, Flow 14b | `repair_recovery_finishes_discard_changes`, `repair_recovery_completes_apply_incoming_fast_forward`, `repair_remote_delete_recovers_after_visible_ref_was_deleted`, `clear_stale_lock_removes_only_stale_metadata_lock` |

| Scenario group | Coverage | Current primitives | Remaining gap |
|---|---|---|---|
| Start or open workspace | Covered | `init`, `open`, `clone_workspace`, `workspace_summary` | None for MVP. |
| Adopt existing non-Draftline repo | Partially covered | `open`, `workspace_summary`, status/history APIs, `inspect`, `preflight_adopt_workspace` | Adoption is read-only and structured, but still needs broader branch/remote/policy migration decisions owned by hosts. |
| Developer Copilot opens a Draftline-managed repo | Partially covered | `generate_agent_instructions`, `inspect`, `verify_workspace`, `explain_error` | Rust helpers exist; no repository instruction file generator or standalone CLI/tool command surface yet. |
| Agent uses Draftline APIs directly | Partially covered | `inspect`, `inspect_json`, `capabilities`, `capabilities_json`, `verify_workspace`, `explain_error`, operation-specific preflights | Rust API/JSON helpers exist; no `draftline inspect --json` CLI, generic execute protocol, or full operation-token facade yet. |
| Configure actor identity | Host concern | Commit signatures through workspace configuration | Need explicit identity diagnostics because authorship, audit, and support-ref naming depend on it. |
| Work local-only before remote exists | Covered | `init`, `open`, local save/variation/history APIs | Publish, apply incoming, and shared support-ref sync are unavailable until a remote is configured. |
| Add remote after local work exists | Partially covered | `add_remote`, `preflight_publish`, `publish`, `publish_changes` | First publish captures expected remote absence/state, but support-ref sync bootstrap and remote destination confirmation are still missing. |
| Start from shared remote | Covered for clone/open and fetched remote variations | `clone_workspace`, `clone_workspace_with_policy_and_options`, `remote_variations`, `adopt_remote_variation` | Need broader fetch-all/prune diagnostics after clone. |
| Configure tracked content | Host concern | `ContentPolicy`, `tracks`, `content_policy`, `resolve_path` | Default policy is permissive except `.draftline`; hosts must choose app-specific policy and account for Git ignore rules. |
| Content policy vs Git ignore rules | Partially covered | Git status plus `ContentPolicy`, `policy_git_diagnostics`, `audit_content_policy` | Current ignored-file warnings exist; attributes/filter/path-normalization/history migration diagnostics remain limited. |
| Change content policy after saves | Not covered | Runtime `ContentPolicy` only | Need policy migration/redaction model; existing versions are not retroactively filtered. |
| Workspace before first save | Partially covered | `changes`, `save_version`, `workspace_summary` | Variation, remote, and history actions may require an initial version. |
| Understand current state | Covered | `workspace_summary`, `changes`, `history`, `full_history`, `variation_summaries`, `sync_status` | During recovery, only `workspace_summary` and `recovery_state` are available; better product copy for unusual Git states. |
| Save business work | Partially covered | `changes`, `save_version`, `ContentPolicy`, `policy_git_diagnostics` | Current ignored-file diagnostics exist; attributes/filter and historical policy diagnostics remain limited. |
| Save or shelve selected work | Covered for selected files | `preflight_save_files`, `save_files`, `preflight_shelve_files`, `shelve_files`, `preflight_discard_files`, `discard_files`, Tauri `selected_*` commands | Current package/Workbench coverage includes selected save, shelf, and discard; richer selected-file conflict UX remains future work. |
| Abandon unsaved edits | Covered | `preflight_discard_changes`, `discard_changes`, `preflight_discard_file`, `discard_file` | Switch-time discard remains intentionally unsupported; discard must be a separate explicit action. |
| Try another direction | Covered | `create_variation`, `create_variation_from`, variation metadata APIs | None for MVP. |
| Rename or relabel direction | Covered | `set_variation_metadata`, `variation_metadata` | Ref renaming is intentionally not exposed. |
| Move between directions | Covered for full-variation switching | `preflight_switch_variation`, `switch_variation`, `AbortIfDirty`, `SaveFirst`, `Shelve`, shelf lifecycle APIs | Selected-file variation switch remains future work. |
| Review older work | Covered | `history`, `full_history`, `diff_versions`, `preview_version`, `preview_version_file` | None for MVP. |
| Restore older work | Partially covered | `restore_version_as_new_save` | Restore blocks dirty work and target-tree collisions; current-policy vs old-policy restore planning still needs a richer report. |
| Target tree collides with local files | Partially covered | Shared `FileHazard` checks in switch, restore, apply incoming, merge incoming, and apply shelf | Ignored and policy-excluded target-path collisions are covered for key file-writing operations; generated, symlink, submodule, case, Unicode, and distinct `Untracked` hazard reporting remain limited. |
| Publish my work | Covered for current variation | `fetch_remote`, `sync_status`, `preflight_publish`, `publish`, `publish_changes` | Tokenized publish detects local/remote state changes after preflight and uses push negotiation to enforce expected remote old/new OIDs before upload. |
| Receive teammate work | Covered for current-variation fast-forward only | `fetch_remote`, `preflight_apply_incoming`, `apply_incoming`, target-tree collision checks | Diverged clean merge is handled by the merge scenario; broad remote lifecycle diagnostics are still missing. |
| We both changed the same workspace | Covered for clean semantic merges | `sync_status`, `SyncNeedsMerge`, semantic resolver types, `preflight_merge_incoming`, `merge_incoming` | Clean semantic merge execution exists; user-driven conflict resolution remains future work. |
| Discover teammate-created variations | Partially covered | `fetch_all_variations`, `remote_variations`, `remote_variation_diagnostics`, `adopt_remote_variation` | Listing/adoption and local-only/remote-only diagnostics exist from fetched/pruned remote-tracking refs; tokenized adoption remains future work. |
| Remote variation deleted or renamed | Partially covered | `fetch_all_variations`, `remote_variation_diagnostics` | Pruned local-only/remote-only diagnostics exist; higher-level rename inference and product messaging remain host work. |
| Change or remove remote destination | Partially covered | `add_remote`, `remotes` | Updating an existing remote URL is silent; need preflight/confirmation/remove APIs. |
| Remove old local direction | Covered | `preflight_delete_variation`, `delete_variation_with_token`, `delete_variation`, `list_support_refs`, `preflight_restore_support_ref`, `restore_support_ref`, `restore_support_ref_as_variation` | Archives first and exposes local/remote-tracking support-ref preflight restore. |
| Remove shared direction for the team | Covered for remote delete | `preflight_delete_remote_variation`, `delete_remote_variation` | Archive-first remote delete exists with negotiated create-only support-ref publish and expected-OID visible ref delete. |
| Clean up local version history | Covered for local squash and milestone compaction | `preflight_squash_versions`, `squash_versions_with_token`, `squash_versions`, `history_compaction_candidates`, `preview_history_cleanup`, `apply_history_cleanup`, `resolve_rewritten_version`, `preflight_undo_history_cleanup`, `undo_history_cleanup` | Archives old tips first, exposes preview/candidate details, writes stale-version maps, replays safe descendants, and supports undo. |
| Replace shared history | Covered for current variation | `preflight_replace_remote_history`, `RemoteHistoryReplaceToken::confirm_shared_history_rewrite`, `replace_remote_history` | Requires explicit confirmation, publishes recovery support refs first, then replaces the visible remote ref with force-with-lease expectations. |
| Publish compacted shared history | Covered for history cleanup compaction | `preview_history_cleanup`, `apply_history_cleanup`, `preflight_history_cleanup_remote_impact`, `preflight_publish_history_cleanup`, `publish_history_cleanup` | Local compaction is applied first, then support refs are published create-only before the remote branch is moved with expected-OID lease protection. |
| Sync compacted shared history | Covered for safe incoming rewrites | `fetch_remote`, `sync_status`, `preflight_apply_incoming`, `apply_incoming`, `resolve_rewritten_version` | Fetch includes support refs; recognized remote compaction appears as incoming, clean peers apply it, local first-parent saves replay, dirty work blocks, and ambiguous merge-shaped histories stay in merge flow. |
| Sync hidden recovery support refs | Covered for local publish/fetch/restore | `preflight_publish_support_refs`, `publish_support_refs`, `fetch_support_refs`, `list_support_refs(RemoteTracking)`, `preflight_restore_support_ref`, `restore_support_ref` | Create-only publication, remote-tracking fetch, and remote-tracking restore preflight exist; remote retention remains future work. |
| Recover cleanup after clone/device loss | Covered for fetched support refs | Local archive listing, remote-tracking support-ref listing, `preflight_restore_support_ref`, `restore_support_ref_as_variation` | Cross-machine discovery and restore from remote-tracking support refs exist after fetch; retention policy remains future work. |
| Permanently purge/redact content | Planning-only | `preflight_purge_content`, `verify_purge` | Planning and limitation reporting exist; no destructive `purge_content` execution workflow. |
| Large or binary business assets | Partially covered | `is_binary`, `is_large`, preview metadata | Need policy for block/warn/stream/external storage. |
| Out-of-band Git mutation | Partially covered | `NoCurrentVariation`, Git errors, status, `inspect`, `verify_workspace`, `explain_error` | Structured diagnostics exist for common states; repair guidance is still limited. |
| Recover from interruption | Partially covered | `RecoveryState`, `workspace_summary.recovery`, operation lock, `acknowledge_recovery`, `repair_recovery`, `rollback_recovery` | Repair/rollback perform metadata-backed recovery for covered operations and return safe blockers when the ledger lacks enough state. |
| Recover stale operation lock | Partially covered | `inspect_operation_lock`, `clear_stale_lock` | Metadata-based stale lock clearing exists; deeper lock/recovery repair coordination remains limited. |
| Bring shelved work back | Covered for all-work shelves | `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, `delete_shelf` | Selected-file shelves and conflict-resolution apply remain future work. |

## Detailed primitive coverage

Primitive rows use the same scoped-status rule: if the status names a scope, consuming apps should depend only on that scope.

| Business action | Status | Primitive coverage | Notes |
|---|---|---|---|
| Open local workspace | Covered | `Workspace::open`, `Workspace::open_with_policy` | Discovers existing repository. |
| Adopt existing repo | Partially covered | `open`, `workspace_summary`, status/history/remote APIs, `inspect`, `preflight_adopt_workspace` | Setup preflight exists; app-specific migration decisions and broader Git-shape diagnostics remain host work. |
| Agent-safe API surface | Partially covered | Rust APIs plus JSON helpers for `inspect` and `capabilities`, `verify_workspace`, `explain_error` | No standalone CLI/tool facade or generic execute protocol yet. |
| Create local workspace | Covered | `Workspace::init`, `Workspace::init_with_policy` | Creates folder/repository if needed. |
| Clone shared workspace | Covered | `Workspace::clone_workspace*` | Remote options allow host-provided credentials. |
| Configure content boundaries | Covered | `ContentPolicy` methods | Include/exclude roots, extensions, and large-file thresholds. |
| Resolve safe path | Covered | `resolve_path` | Rejects absolute and escaping paths. |
| Show dashboard | Covered | `workspace_summary` | Includes active variation, versions, dirty files, and recovery. |
| Show changes | Partially covered | `changed_files`, `changes`, `is_dirty`, `policy_git_diagnostics` | Content policy filters non-user files and ignored policy-tracked files can be diagnosed; attributes/filter diagnostics are still limited. |
| Save version | Partially covered | `save_version`, `policy_git_diagnostics` | App supplies user-facing label; ignored policy-tracked files can be diagnosed, but attributes/filter hazards still need richer reporting. |
| Discard unsaved edits | Covered | `preflight_discard_changes`, `discard_changes`, `preflight_discard_file`, `discard_file` | Explicit and content-policy-aware. |
| Create option | Covered | `create_variation`, `create_variation_from` | Does not expose detached HEAD. |
| Label option | Covered | `VariationMetadata`, `set_variation_metadata`, `variation_metadata` | Display metadata does not rename Git refs. |
| List options | Covered | `variations`, `variation_summaries` | Summary avoids switching variations. |
| Switch option | Covered for full-variation switching | `preflight_switch_variation`, `switch_variation` | Dirty-work policies and key target-tree collision checks are explicit; selected-file flows remain future work. |
| Put work aside | Covered for all-work shelves | `SwitchPolicy::Shelve`, `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, `delete_shelf` | All-work shelf lifecycle exists; selected-file shelves and share-shelf policy remain future work. |
| Preview old work | Covered | `preview_version`, `preview_version_file` | Read-only. |
| Compare versions | Covered | `diff_versions`, `diff_version_to_workspace` | Read-only; version-to-version diffs are historical tree comparisons, not policy redaction. |
| Restore old version | Partially covered | `restore_version_as_new_save` | Append-only restore and target-tree collision checks exist; richer policy-aware old-tree restore planning remains missing. |
| Add/list remotes | Covered | `add_remote`, `remotes` | Host owns remote naming and URL UX. |
| Fetch remote state | Covered | `fetch_remote`, `fetch_remote_with_options` | Fetches current variation. |
| Show sync state | Covered | `sync_status` | Reports ahead/behind and incoming summaries. |
| Publish | Covered for current variation | `publish_changes`, `publish_changes_with_options`, `preflight_publish`, `publish` | Tokenized publish captures expected state, rejects changed local/remote state, and enforces expected remote old/new OIDs through push negotiation. |
| Get updates | Covered for current-variation fast-forward only | `preflight_apply_incoming`, `apply_incoming` | Dirty, diverged, and key target-tree collision states block. |
| Sync recovery support refs | Covered for local publish/fetch | `preflight_publish_support_refs`, `publish_support_refs`, `fetch_support_refs`, `list_support_refs(RemoteTracking)` | Create-only pushes and remote-tracking fetch layout exist; remote retention and direct remote restore planning remain future work. |
| Merge teammate changes | Covered for clean semantic merges | `SyncNeedsMerge`, merge resolver model, `preflight_merge_incoming`, `merge_incoming` | Clean merge execution exists; unresolved conflict resolution workflow remains future work. |
| Apply shelved work | Covered for clean all-work shelf apply | `preview_shelf`, `preflight_apply_shelf`, `apply_shelf` | Conflict-resolution apply for dirty/diverged shelf scenarios remains future work. |
| Delete local old option | Covered | `preflight_delete_variation`, `delete_variation_with_token`, `delete_variation`, `list_support_refs`, `preflight_restore_support_ref`, `restore_support_ref_as_variation` | Archives first and exposes preflight details before deletion. |
| Delete shared old option | Partially covered | `preflight_delete_remote_variation`, `delete_remote_variation`, `fetch_support_refs`, `list_support_refs(RemoteTracking)` | Archive-first remote delete exists with negotiated create-only support-ref publish and expected-OID visible ref delete; remote lifecycle diagnostics remain incomplete. |
| Squash local history | Covered for local squash | `preflight_squash_versions`, `squash_versions_with_token`, `squash_versions` | Archives first and exposes the archive plan before rewrite. |
| Replace shared history | Covered for current variation | `preflight_replace_remote_history`, `RemoteHistoryReplaceToken::confirm_shared_history_rewrite`, `replace_remote_history` | Explicitly confirmed, support-ref-first force-with-lease replacement exists for the current variation. |
| Recover interrupted operation | Partially covered | `RecoveryState`, `recovery_state`, `acknowledge_recovery`, `inspect_operation_lock`, `clear_stale_lock`, `repair_recovery`, `rollback_recovery` | Stale-lock handling and metadata-backed repair/rollback exist; operations whose ledger lacks enough state still return a repair blocker. |

## Follow-up roadmap

| Priority | Gap | Why it matters | Candidate primitives |
|---|---|---|---|
| 1 | Recovery metadata expansion | Some interrupted operations still cannot be safely repaired because the ledger does not capture their pre-operation OIDs or remote substeps. | Expand `RecoveryState` for restore and remote delete repair/rollback |
| 2 | Remote support-ref retention | Support refs can be published/fetched/restored from remote-tracking refs, but remote expiration policy is not implemented. | Remote retention APIs |
| 3 | Agent CLI/tool facade completion | Coding agents have JSON inspect/capabilities/verify/explain-error commands; generic preflight/execute and recovery commands remain future work. | `draftline preflight`, `execute`, `recovery diagnose`, `repair`, `rollback`, `clear-stale-lock` |
| 4 | Conflict resolution merge execution | Collaboration can execute clean semantic merges, but users still need a safe path for unresolved conflicts. | `MergeResolution` execution |
| 5 | ContentPolicy and Git metadata expansion | Attributes, filters, path normalization, and filesystem behavior can hide or transform business content. | Broader policy/Git audit in `changes`, adoption, restore, and switch preflights |
| 6 | Shared cleanup history replacement expansion | Current-variation replacement requires explicit confirmation; broader consent models and multi-variation/admin workflows remain future work. | Admin UX and multi-variation policy |
| 7 | Remote variation lifecycle completion | Teammate-created/deleted variations can be diagnosed after fetch/prune; rename inference and tokenized adoption remain future work. | Rename diagnostics, `preflight_adopt_remote_variation` |
| 8 | Purge/redaction execution | Planning exists, but true deletion must enumerate all refs/reflogs and communicate distributed-Git limits. | Explicit destructive best-effort `purge_content` workflow |
| 9 | Selected-file conflict expansion | Selected save, shelf, and batch discard exist; richer selected-file conflict UX remains future work. | Selected-file conflict reports |
| 10 | Content policy migration/redaction | Runtime policy changes do not remove or reclassify previously saved content. | `audit_policy`, `migrate_policy`, `redact_content` |
| 11 | Cleanup preflight expansion | Delete and squash preflights exist; richer host confirmation copy remains future work. | Product copy and confirmation UX |
| 12 | Large/binary asset policy | Detection alone does not prevent repo bloat or unsupported merge UX. | Asset policy, external storage, LFS-like integration |
| 13 | Actor identity and authorship | Commit attribution, audit copy, and support-ref naming need a clear actor/device source. | Identity diagnostics and host-provided actor/device metadata |
| 14 | Product diagnostics | Host apps need clearer guidance for unusual Git states. | Structured diagnostic report over raw `git2` errors |
