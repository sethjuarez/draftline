# Draftline coverage and roadmap

[Back to scenario index](scenarios.md)

## Executive scenario coverage

| Scenario group | Coverage | Current primitives | Remaining gap |
|---|---|---|---|
| Start or open workspace | Covered | `init`, `open`, `clone_workspace`, `workspace_summary` | None for MVP. |
| Adopt existing non-Draftline repo | Partially covered | `open`, `workspace_summary`, status/history APIs, `inspect`, `preflight_adopt_workspace` | Adoption is read-only and structured, but still needs broader branch/remote/policy migration decisions owned by hosts. |
| Developer Copilot opens a Draftline-managed repo | Partially covered | `generate_agent_instructions`, `inspect`, `verify_workspace`, `explain_error` | Rust helpers exist; no repository instruction file generator or standalone CLI/tool command surface yet. |
| Agent uses Draftline APIs directly | Partially covered | `inspect`, `inspect_json`, `capabilities`, `capabilities_json`, `verify_workspace`, `explain_error`, operation-specific preflights | Rust API/JSON helpers exist; no `draftline inspect --json` CLI, generic execute protocol, or full operation-token facade yet. |
| Configure actor identity | Host concern | Commit signatures through workspace configuration | Need explicit identity diagnostics because authorship, audit, and support-ref naming depend on it. |
| Work local-only before remote exists | Covered | `init`, `open`, local save/variation/history APIs | Publish, apply incoming, and shared support-ref sync are unavailable until a remote is configured. |
| Add remote after local work exists | Partially covered | `add_remote`, `preflight_publish`, `publish`, `publish_changes` | First publish captures expected remote absence/state, but support-ref sync bootstrap and remote destination confirmation are still missing. |
| Start from shared remote | Covered for clone/open | `clone_workspace`, `clone_workspace_with_policy_and_options`, `remote_variations`, `adopt_remote_variation` | Need broader fetch-all/prune diagnostics after clone. |
| Configure tracked content | Host concern | `ContentPolicy`, `tracks`, `content_policy`, `resolve_path` | Default policy is permissive except `.draftline`; hosts must choose app-specific policy and account for Git ignore rules. |
| Content policy vs Git ignore rules | Partially covered | Git status plus `ContentPolicy`, `policy_git_diagnostics`, `audit_content_policy` | Current ignored-file warnings exist; attributes/filter/path-normalization/history migration diagnostics remain limited. |
| Change content policy after saves | Not covered | Runtime `ContentPolicy` only | Need policy migration/redaction model; existing versions are not retroactively filtered. |
| Workspace before first save | Partially covered | `changes`, `save_version`, `workspace_summary` | Variation, remote, and history actions may require an initial version. |
| Understand current state | Covered | `workspace_summary`, `changes`, `history`, `full_history`, `variation_summaries`, `sync_status` | During recovery, only `workspace_summary` and `recovery_state` are available; better product copy for unusual Git states. |
| Save business work | Partially covered | `changes`, `save_version`, `ContentPolicy`, `policy_git_diagnostics` | Current ignored-file diagnostics exist; attributes/filter and historical policy diagnostics remain limited. |
| Save or shelve selected work | Not covered | `save_version`, `SwitchPolicy::Shelve` affect all tracked dirty files | Need partial save/shelf APIs. |
| Abandon unsaved edits | Covered | `preflight_discard_changes`, `discard_changes`, `preflight_discard_file`, `discard_file` | Switch-time discard remains intentionally unsupported; discard must be a separate explicit action. |
| Try another direction | Covered | `create_variation`, `create_variation_from`, variation metadata APIs | None for MVP. |
| Rename or relabel direction | Covered | `set_variation_metadata`, `variation_metadata` | Ref renaming is intentionally not exposed. |
| Move between directions | Covered for MVP | `preflight_switch_variation`, `switch_variation`, `AbortIfDirty`, `SaveFirst`, `Shelve`, shelf lifecycle APIs | Switching preflights dirty work and target-tree collisions; partial selected-file switch/shelf remains future work. |
| Review older work | Covered | `history`, `full_history`, `diff_versions`, `preview_version`, `preview_version_file` | None for MVP. |
| Restore older work | Partially covered | `restore_version_as_new_save` | Restore blocks dirty work and target-tree collisions; current-policy vs old-policy restore planning still needs a richer report. |
| Target tree collides with local files | Partially covered | Shared `FileHazard` checks in switch, restore, apply incoming, and apply shelf | Ignored and policy-excluded target-path collisions are covered for key file-writing operations; generated, symlink, submodule, case, Unicode, and distinct `Untracked` hazard reporting remain limited. |
| Publish my work | Partially covered | `fetch_remote`, `sync_status`, `preflight_publish`, `publish`, `publish_changes` | Tokenized publish detects local/remote state changes after preflight; push refspecs still rely on server rejection rather than explicit lease/create-only refspecs. |
| Receive teammate work | Covered for current-variation fast-forward | `fetch_remote`, `preflight_apply_incoming`, `apply_incoming`, target-tree collision checks | Diverged merge execution and broad remote lifecycle diagnostics are still missing. |
| We both changed the same workspace | Partially covered | `sync_status`, `SyncNeedsMerge`, semantic resolver types, `preflight_merge_incoming` | Public read-only merge preflight exists; no public `merge_incoming` execution workflow yet. |
| Discover teammate-created variations | Partially covered | `remote_variations`, `adopt_remote_variation` | Listing/adoption exist from fetched remote-tracking refs; fetch-all and stale/prune diagnostics remain missing. |
| Remote variation deleted or renamed | Not covered | `fetch_remote` ignores missing remote refs | Need prune/stale-ref diagnostics and product messaging. |
| Change or remove remote destination | Partially covered | `add_remote`, `remotes` | Updating an existing remote URL is silent; need preflight/confirmation/remove APIs. |
| Remove old local direction | Partially covered | `delete_variation` archives tip first, `list_support_refs`, `restore_support_ref_as_variation` | Archive listing and restore exist locally; delete preflight and shared support-ref sync remain missing. |
| Remove shared direction for the team | Partially covered | `preflight_delete_remote_variation`, `delete_remote_variation` | Archive-first remote delete exists, but explicit lease/create-only push semantics and remote support-ref fetch/list flows remain incomplete. |
| Clean up local version history | Partially covered | `squash_versions` archives old tip first | Published branches cannot use normal `publish_changes` after squash because Draftline does not force-push. |
| Replace shared history | Not covered | Local squash only | Needs explicit replace-remote-history workflow with consent, support-ref publish, and force-with-lease semantics. |
| Sync hidden recovery support refs | Not covered | Local `refs/draftline/...` refs only; remote delete publishes one support ref as part of that operation | Need general support-ref fetch/publish refspecs for the shared remote. |
| Recover cleanup after clone/device loss | Partially covered | Local archive listing and `restore_support_ref_as_variation` | Cross-machine recovery still needs shared support-ref sync. |
| Permanently purge/redact content | Partially covered | `preflight_purge_content`, `verify_purge` | Planning and limitation reporting exist; no destructive `purge_content` execution workflow. |
| Large or binary business assets | Partially covered | `is_binary`, `is_large`, preview metadata | Need policy for block/warn/stream/external storage. |
| Out-of-band Git mutation | Partially covered | `NoCurrentVariation`, Git errors, status, `inspect`, `verify_workspace`, `explain_error` | Structured diagnostics exist for common states; repair guidance is still limited. |
| Recover from interruption | Partially covered | `RecoveryState`, `workspace_summary.recovery`, operation lock, `acknowledge_recovery`, `repair_recovery`, `rollback_recovery` skeletons | Typed repair/rollback entry points report state but do not yet perform operation-specific mutation. |
| Recover stale operation lock | Partially covered | `inspect_operation_lock`, `clear_stale_lock` | Metadata-based stale lock clearing exists; deeper lock/recovery repair coordination remains limited. |
| Bring shelved work back | Covered for MVP | `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, `delete_shelf` | Selected-file shelves and conflict-resolution apply remain future work. |

## Detailed primitive coverage

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
| Switch option | Covered for MVP | `preflight_switch_variation`, `switch_variation` | Dirty-work policies and key target-tree collision checks are explicit; selected-file flows remain future work. |
| Put work aside | Covered for MVP | `SwitchPolicy::Shelve`, `shelve_changes`, `list_shelves`, `preview_shelf`, `preflight_apply_shelf`, `apply_shelf`, `delete_shelf` | All-work shelf lifecycle exists; selected-file shelves and share-shelf policy remain future work. |
| Preview old work | Covered | `preview_version`, `preview_version_file` | Read-only. |
| Compare versions | Covered | `diff_versions`, `diff_version_to_workspace` | Read-only; version-to-version diffs are historical tree comparisons, not policy redaction. |
| Restore old version | Partially covered | `restore_version_as_new_save` | Append-only restore and target-tree collision checks exist; richer policy-aware old-tree restore planning remains missing. |
| Add/list remotes | Covered | `add_remote`, `remotes` | Host owns remote naming and URL UX. |
| Fetch remote state | Covered | `fetch_remote`, `fetch_remote_with_options` | Fetches current variation. |
| Show sync state | Covered | `sync_status` | Reports ahead/behind and incoming summaries. |
| Publish | Partially covered | `publish_changes`, `publish_changes_with_options`, `preflight_publish`, `publish` | Tokenized publish captures expected state and rejects changed local/remote state; explicit lease/create-only push mechanics still need tightening. |
| Get updates | Covered for fast-forward | `preflight_apply_incoming`, `apply_incoming` | Dirty, diverged, and key target-tree collision states block. |
| Sync recovery support refs | Not covered | Local `refs/draftline/...` only | Needs shared-remote support-ref refspecs, create-only pushes, and remote-tracking layout. |
| Merge teammate changes | Partially covered | `SyncNeedsMerge`, merge resolver model, `preflight_merge_incoming` | Needs public merge execution workflow. |
| Apply shelved work | Covered for MVP | `preview_shelf`, `preflight_apply_shelf`, `apply_shelf` | Conflict-resolution apply for dirty/diverged shelf scenarios remains future work. |
| Delete local old option | Partially covered | `delete_variation`, `list_support_refs`, `restore_support_ref_as_variation` | Archives first and local restore exists; preflight delete remains missing. |
| Delete shared old option | Partially covered | `preflight_delete_remote_variation`, `delete_remote_variation` | Archive-first remote delete exists; general support-ref sync and explicit lease/create-only mechanics remain incomplete. |
| Squash local history | Partially covered | `squash_versions` | Archives first; preflight and restore archive APIs missing. |
| Replace shared history | Not covered | None | Needs explicit replace workflow; normal publish rejects non-fast-forward squashed history. |
| Recover interrupted operation | Partially covered | `RecoveryState`, `recovery_state`, `acknowledge_recovery`, `inspect_operation_lock`, `clear_stale_lock`, `repair_recovery`, `rollback_recovery` | Stale-lock handling and typed repair/rollback reports exist; operation-specific repair/rollback mutation is still missing. |

## Follow-up roadmap

| Priority | Gap | Why it matters | Candidate primitives |
|---|---|---|---|
| 1 | Operation-specific recovery repair | Acknowledge and skeleton repair reports are not enough for interrupted ref-moving operations. | Implement mutation in `repair_recovery` and `rollback_recovery` per operation kind |
| 2 | Explicit lease/create-only push mechanics | Fetch-then-compare plus normal push still depends on server rejection for some post-fetch races. | Lease/create-only refspec support for publish and shared delete support-ref publication |
| 3 | General support-ref sync | Shared recovery requires hidden `refs/draftline/...` support refs to travel across machines safely. | `publish_support_refs`, `fetch_support_refs`, remote-tracking support-ref layout |
| 4 | Agent CLI/tool facade | Coding agents need direct access to Draftline's safety model outside embedded Rust callers. | `draftline inspect`, `capabilities`, `preflight`, `execute`, `verify`, `recovery diagnose`, JSON result schema |
| 5 | Diverged merge execution | Collaboration cannot stop at "needs merge"; users need a safe resolution path. | `merge_incoming`, `MergeResolution` execution |
| 6 | ContentPolicy and Git metadata expansion | Attributes, filters, path normalization, and filesystem behavior can hide or transform business content. | Broader policy/Git audit in `changes`, adoption, restore, and switch preflights |
| 7 | Shared cleanup history replacement | Local squash does not replace shared remote history, and normal publish rejects rewritten shared history. | `preflight_replace_remote_history`, `replace_remote_history` |
| 8 | Remote variation lifecycle completion | Teammate-created variations can be listed/adopted, but deleted or renamed remote variations still need product diagnostics. | `fetch_all_variations`, prune/stale diagnostics |
| 9 | Purge/redaction execution | Planning exists, but true deletion must enumerate all refs/reflogs and communicate distributed-Git limits. | Explicit destructive best-effort `purge_content` workflow |
| 10 | Partial save and shelf | Users need to save or put aside selected work without forcing all dirty files into one action. | `preflight_save_files`, `save_files`, `shelve_files`, `discard_files` |
| 11 | Content policy migration/redaction | Runtime policy changes do not remove or reclassify previously saved content. | `audit_policy`, `migrate_policy`, `redact_content` |
| 12 | Preflight delete and squash | Users should know what will be archived before cleanup. | `preflight_delete_variation`, `preflight_squash_versions` |
| 13 | Large/binary asset policy | Detection alone does not prevent repo bloat or unsupported merge UX. | Asset policy, external storage, LFS-like integration |
| 14 | Actor identity and authorship | Commit attribution, audit copy, and support-ref naming need a clear actor/device source. | Identity diagnostics and host-provided actor/device metadata |
| 15 | Product diagnostics | Host apps need clearer guidance for unusual Git states. | Structured diagnostic report over raw `git2` errors |
