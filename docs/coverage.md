# Draftline coverage and roadmap

[Back to scenario index](scenarios.md)

## Executive scenario coverage

| Scenario group | Coverage | Current primitives | Remaining gap |
|---|---|---|---|
| Start or open workspace | Covered | `init`, `open`, `clone_workspace`, `workspace_summary` | None for MVP. |
| Adopt existing non-Draftline repo | Partially covered | `open`, `workspace_summary`, status/history APIs | Need setup preflight that maps branches/remotes/content policy into Draftline concepts without mutating first. |
| Developer Copilot opens a Draftline-managed repo | Not covered | Informal docs only | Need agent-facing instructions that explain safe direct Git interaction and Draftline-owned state. |
| Agent uses Draftline APIs directly | Not covered | Library APIs only | Need agent-friendly API/CLI shape with discovery, preflight, execute, verify, and repair operations. |
| Configure actor identity | Host concern | Commit signatures through workspace configuration | Need explicit identity diagnostics because authorship, audit, and support-ref naming depend on it. |
| Work local-only before remote exists | Covered | `init`, `open`, local save/variation/history APIs | Publish, apply incoming, and shared support-ref sync are unavailable until a remote is configured. |
| Add remote after local work exists | Partially covered | `add_remote`, `publish_changes` | Need preflight that explains first publish, remote branch conflicts, and support-ref sync bootstrap. |
| Start from shared remote | Covered for clone/open | `clone_workspace`, `clone_workspace_with_policy_and_options` | Need broader remote variation discovery after clone. |
| Configure tracked content | Host concern | `ContentPolicy`, `tracks`, `content_policy`, `resolve_path` | Default policy is permissive except `.draftline`; hosts must choose app-specific policy and account for Git ignore rules. |
| Content policy vs Git ignore rules | Partially covered | Git status plus `ContentPolicy` | Need warnings for policy-tracked files hidden by `.gitignore` or attributes. |
| Change content policy after saves | Not covered | Runtime `ContentPolicy` only | Need policy migration/redaction model; existing versions are not retroactively filtered. |
| Workspace before first save | Partially covered | `changes`, `save_version`, `workspace_summary` | Variation, remote, and history actions may require an initial version. |
| Understand current state | Covered | `workspace_summary`, `changes`, `history`, `full_history`, `variation_summaries`, `sync_status` | During recovery, only `workspace_summary` and `recovery_state` are available; better product copy for unusual Git states. |
| Save business work | Partially covered | `changes`, `save_version`, `ContentPolicy` | Need diagnostics for policy-tracked files hidden by Git ignore/attributes. |
| Save or shelve selected work | Not covered | `save_version`, `SwitchPolicy::Shelve` affect all tracked dirty files | Need partial save/shelf APIs. |
| Abandon unsaved edits | Covered | `preflight_discard_changes`, `discard_changes`, `preflight_discard_file`, `discard_file` | Switch-time discard remains intentionally unsupported; discard must be a separate explicit action. |
| Try another direction | Covered | `create_variation`, `create_variation_from`, variation metadata APIs | None for MVP. |
| Rename or relabel direction | Covered | `set_variation_metadata`, `variation_metadata` | Ref renaming is intentionally not exposed. |
| Move between directions | Partially covered | `preflight_switch_variation`, `switch_variation`, `AbortIfDirty`, `SaveFirst`, `Shelve` | `SaveFirst` is covered; shelve is incomplete without list/preview/apply/delete APIs. |
| Review older work | Covered | `history`, `full_history`, `diff_versions`, `preview_version`, `preview_version_file` | None for MVP. |
| Restore older work | Partially covered | `restore_version_as_new_save` | Need target-tree collision and current-policy preflight for old-policy files. |
| Target tree collides with local files | Not covered | File-writing operations vary | Need preflight for untracked, ignored, generated, and policy-excluded files before checkout/restore/apply/switch. |
| Publish my work | Partially covered | `fetch_remote`, `sync_status`, `publish_changes` | Need expected-OID/lease semantics for first publish and remote branch races. |
| Receive teammate work | Covered for current-variation fast-forward | `fetch_remote`, `preflight_apply_incoming`, `apply_incoming` | Diverged merge flow, teammate-created variation discovery, and broad target-tree collision preflight are missing. |
| We both changed the same workspace | Partially covered | `sync_status`, `SyncNeedsMerge`, semantic resolver types | No public `merge_incoming` workflow yet. |
| Discover teammate-created variations | Not covered | Current-variation fetch/status only | Need remote variation listing/import/adoption APIs. |
| Remote variation deleted or renamed | Not covered | `fetch_remote` ignores missing remote refs | Need prune/stale-ref diagnostics and product messaging. |
| Change or remove remote destination | Partially covered | `add_remote`, `remotes` | Updating an existing remote URL is silent; need preflight/confirmation/remove APIs. |
| Remove old local direction | Partially covered | `delete_variation` archives tip first | Need preflight and restore-from-archive APIs. |
| Remove shared direction for the team | Not covered | Local delete only | Need remote-safe delete with archive-first, support-ref publish, and expected-OID remote deletion. |
| Clean up local version history | Partially covered | `squash_versions` archives old tip first | Published branches cannot use normal `publish_changes` after squash because Draftline does not force-push. |
| Replace shared history | Not covered | Local squash only | Needs explicit replace-remote-history workflow with consent, support-ref publish, and force-with-lease semantics. |
| Sync hidden recovery support refs | Not covered | Local `refs/draftline/...` refs only | Need support-ref fetch/publish refspecs for the shared remote. |
| Recover cleanup after clone/device loss | Not covered | Local archive refs only | Needs shared support-ref sync plus archive listing/restore APIs. |
| Permanently purge/redact content | Not covered | Archive refs intentionally retain content | Need explicit best-effort purge/redaction workflow separate from cleanup. |
| Large or binary business assets | Partially covered | `is_binary`, `is_large`, preview metadata | Need policy for block/warn/stream/external storage. |
| Out-of-band Git mutation | Partially covered | `NoCurrentVariation`, Git errors, status | Need structured diagnostics and repair guidance. |
| Recover from interruption | Partially covered | `RecoveryState`, `workspace_summary.recovery`, operation lock, `acknowledge_recovery` | Need operation-specific repair or rollback APIs. |
| Recover stale operation lock | Not covered | `WorkspaceLocked` only | Need stale-lock detection and guarded unlock/repair flow. |
| Bring shelved work back | Not covered | Shelf refs are created internally | Need shelf listing, preview, apply, and delete APIs. |

## Detailed primitive coverage

| Business action | Status | Primitive coverage | Notes |
|---|---|---|---|
| Open local workspace | Covered | `Workspace::open`, `Workspace::open_with_policy` | Discovers existing repository. |
| Adopt existing repo | Partially covered | `open`, `workspace_summary`, status/history/remote APIs | Needs setup preflight for non-Draftline repo shape, policy fit, remotes, and blockers. |
| Agent-safe API surface | Not covered | Rust primitives only | Needs JSON/CLI/tool facade with inspect, preflight, execute, verify, and recovery operations. |
| Create local workspace | Covered | `Workspace::init`, `Workspace::init_with_policy` | Creates folder/repository if needed. |
| Clone shared workspace | Covered | `Workspace::clone_workspace*` | Remote options allow host-provided credentials. |
| Configure content boundaries | Covered | `ContentPolicy` methods | Include/exclude roots, extensions, and large-file thresholds. |
| Resolve safe path | Covered | `resolve_path` | Rejects absolute and escaping paths. |
| Show dashboard | Covered | `workspace_summary` | Includes active variation, versions, dirty files, and recovery. |
| Show changes | Partially covered | `changed_files`, `changes`, `is_dirty` | Content policy filters non-user files; ignored policy-tracked files need diagnostics. |
| Save version | Partially covered | `save_version` | App supplies user-facing label; ignored policy-tracked files can be missed without policy/Git diagnostics. |
| Discard unsaved edits | Covered | `preflight_discard_changes`, `discard_changes`, `preflight_discard_file`, `discard_file` | Explicit and content-policy-aware. |
| Create option | Covered | `create_variation`, `create_variation_from` | Does not expose detached HEAD. |
| Label option | Covered | `VariationMetadata`, `set_variation_metadata`, `variation_metadata` | Display metadata does not rename Git refs. |
| List options | Covered | `variations`, `variation_summaries` | Summary avoids switching variations. |
| Switch option | Partially covered | `preflight_switch_variation`, `switch_variation` | Dirty-work policies are explicit; target-tree collision preflight needs broadening. |
| Put work aside | Partially covered | `SwitchPolicy::Shelve` | Shelf creation exists, but shelf management APIs are missing. |
| Preview old work | Covered | `preview_version`, `preview_version_file` | Read-only. |
| Compare versions | Covered | `diff_versions`, `diff_version_to_workspace` | Read-only; version-to-version diffs are historical tree comparisons, not policy redaction. |
| Restore old version | Partially covered | `restore_version_as_new_save` | Append-only restore; needs policy-aware restore and target-tree collision preflight. |
| Add/list remotes | Covered | `add_remote`, `remotes` | Host owns remote naming and URL UX. |
| Fetch remote state | Covered | `fetch_remote`, `fetch_remote_with_options` | Fetches current variation. |
| Show sync state | Covered | `sync_status` | Reports ahead/behind and incoming summaries. |
| Publish | Partially covered | `publish_changes`, `publish_changes_with_options` | Fetches before push and refuses unsafe states; needs expected-OID/lease semantics for remote races. |
| Get updates | Covered for fast-forward | `preflight_apply_incoming`, `apply_incoming` | Dirty or diverged states block; target-tree collision preflight needs broadening. |
| Sync recovery support refs | Not covered | Local `refs/draftline/...` only | Needs shared-remote support-ref refspecs, create-only pushes, and remote-tracking layout. |
| Merge teammate changes | Partially covered | `SyncNeedsMerge`, merge resolver model | Needs public merge workflow. |
| Apply shelved work | Not covered | Shelf refs only | Needs shelf preview/apply conflict workflow. |
| Delete local old option | Partially covered | `delete_variation` | Archives first; remote-safe delete and restore archive APIs missing. |
| Delete shared old option | Not covered | None | Needs archive-first support-ref publish and expected-OID remote ref deletion. |
| Squash local history | Partially covered | `squash_versions` | Archives first; preflight and restore archive APIs missing. |
| Replace shared history | Not covered | None | Needs explicit replace workflow; normal publish rejects non-fast-forward squashed history. |
| Recover interrupted operation | Partially covered | `RecoveryState`, `recovery_state`, `acknowledge_recovery` | Repair, rollback, stale-lock handling, and compound-operation recovery APIs missing. |

## Follow-up roadmap

| Priority | Gap | Why it matters | Candidate primitives |
|---|---|---|---|
| 1 | Stale lock and recovery repair | Acknowledge is not enough for interrupted ref-moving operations; abandoned locks can block all mutation after a crash. | `inspect_operation_lock`, `clear_stale_lock`, `repair_recovery`, `rollback_recovery` |
| 2 | Target-tree collision preflight | File-writing operations must not overwrite untracked, ignored, generated, or policy-excluded files. | Shared checkout/target-tree preflight used by switch, restore, apply, and merge |
| 3 | ContentPolicy and Git metadata diagnostics | Git ignore, attributes, filters, path normalization, and filesystem behavior can hide or transform business content. | Policy/Git audit in `changes`, `preflight_adopt_workspace`, and restore/switch preflights |
| 4 | Agent-safe API/tool surface | Coding agents need direct access to Draftline's safety model instead of falling back to raw Git. | `inspect`, `capabilities`, `preflight`, `execute`, `verify`, `recovery diagnose`, JSON result schema |
| 5 | Adoption/setup preflight | Existing Git repos may not align with Draftline's branch, remote, content-policy, identity, or workspace-state assumptions. | `preflight_adopt_workspace`, setup diagnostics |
| 6 | Publish and remote race leases | Fetch-then-push is not enough if the remote branch is deleted, recreated, or rewound after fetch. | Expected-OID push, create-only first publish, remote-race result types |
| 7 | Support-ref sync and archive recovery | Shared recovery is simpler if hidden `refs/draftline/...` support refs travel with the shared remote, but only if names are unique, immutable, and race-safe. | `publish_support_refs`, `fetch_support_refs`, `list_archived_refs`, `restore_archived_ref_as_variation` |
| 8 | Shared cleanup and history replacement | Local delete/squash does not remove or replace shared remote refs, and normal publish rejects rewritten shared history. | `preflight_delete_remote_variation`, `delete_remote_variation`, `preflight_replace_remote_history`, `replace_remote_history` |
| 9 | Diverged merge workflow | Collaboration cannot stop at "needs merge"; users need a safe resolution path. | `preflight_merge_incoming`, `merge_incoming`, `MergeResolution` |
| 10 | Shelf lifecycle and apply conflicts | Shelving is only half of the business promise without list, preview, apply, conflict handling, and delete. | `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, `delete_shelf`, optional `share_shelf` |
| 11 | Remote variation lifecycle | Teammate-created, deleted, or renamed variations are invisible today. | `remote_variations`, `fetch_all_variations`, `adopt_remote_variation`, prune diagnostics |
| 12 | Purge/redaction across all refs | Support-ref sync improves recovery but increases retention; true deletion must enumerate all refs/reflogs and cannot guarantee deletion from existing clones. | Explicit destructive best-effort purge workflow |
| 13 | Content policy migration/redaction | Runtime policy changes do not remove or reclassify previously saved content. | `audit_policy`, `migrate_policy`, `redact_content` |
| 14 | Preflight delete and squash | Users should know what will be archived before cleanup. | `preflight_delete_variation`, `preflight_squash_versions` |
| 15 | Partial save and shelf | Users need to save or put aside selected work without forcing all dirty files into one action. | `preflight_save_files`, `save_files`, `shelve_files`, `discard_files` |
| 16 | Support-ref retention | Shared support refs can grow indefinitely, but expiration is not the same as sensitive-content purge. | `list_support_refs`, `preflight_expire_support_refs`, `expire_support_refs` |
| 17 | Large/binary asset policy | Detection alone does not prevent repo bloat or unsupported merge UX. | Asset policy, external storage, LFS-like integration |
| 18 | Actor identity and authorship | Commit attribution, audit copy, and support-ref naming need a clear actor/device source. | Identity diagnostics and host-provided actor/device metadata |
| 19 | Remote history replacement details | Lease-protected remote history replacement needs clearer business explanation. | Extended `SyncState` or `SyncStatus` diagnostics |
| 20 | Product diagnostics | Host apps need clearer guidance for unusual Git states. | Structured diagnostic report over raw `git2` errors |
