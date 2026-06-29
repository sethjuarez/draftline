import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { listen as tauriListen } from '@tauri-apps/api/event';

export type DraftlineInvoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
export type DraftlineUnlisten = () => void;
export type DraftlineListen = <T>(
  event: string,
  handler: (event: { payload: T }) => void,
) => Promise<DraftlineUnlisten>;
export type DraftlineEventHandler = (event: DraftlineEvent) => void;

export interface DraftlineClientOptions {
  invoke?: DraftlineInvoke;
  listen?: DraftlineListen;
  workspaceEventName?: string;
  onCommandStart?: (command: string, args?: Record<string, unknown>) => void;
  onCommandSuccess?: (command: string, result: unknown) => void;
  onCommandError?: (command: string, error: unknown) => void;
}

export interface DraftlineClient {
  openWorkspace(workspacePath: string): Promise<WorkspaceOpenResult>;
  cloneWorkspace(request: CloneWorkspaceRequest): Promise<WorkspaceOpenResult>;
  adoptWorkspace(workspacePath: string): Promise<AdoptWorkspaceResult>;
  inspectWorkspace(workspacePath: string): Promise<WorkspaceDiagnostics>;
  verifyWorkspace(workspacePath: string): Promise<WorkspaceVerification>;
  listVariations(workspacePath: string): Promise<VariationSummary[]>;
  preflightSwitchVariation(request: SwitchVariationRequest): Promise<PreflightReport>;
  switchVariation(request: SwitchVariationRequest): Promise<SwitchVariationResult>;
  preflightRenameVariation(request: RenameVariationRequest): Promise<VariationRenamePreflight>;
  renameVariation(request: RenameVariationRequest): Promise<RenameVariationResult>;
  listSupportRefs(workspacePath: string, scope: SupportRefScope): Promise<SupportRef[]>;
  listRemotes(workspacePath: string): Promise<RemoteEndpoint[]>;
  listRemoteVariations(request: RemoteRequest): Promise<RemoteVariation[]>;
  remoteVariationDiagnostics(request: RemoteRequest): Promise<RemoteVariationDiagnostics>;
  adoptRemoteVariation(request: RemoteVariationRequest): Promise<AdoptRemoteVariationResult>;
  getChanges(workspacePath: string): Promise<ChangeSet>;
  getHistory(workspacePath: string): Promise<HistoryEntry[]>;
  getFullHistory(workspacePath: string): Promise<HistoryEntry[]>;
  previewHistoryCleanup(request: PreviewHistoryCleanupRequest): Promise<HistoryCleanupPreview>;
  applyHistoryCleanup(request: ApplyHistoryCleanupRequest): Promise<TimelineCleanupResult>;
  resolveRewrittenVersion(
    request: ResolveRewrittenVersionRequest,
  ): Promise<StaleVersionResolution>;
  preflightUndoHistoryCleanup(
    request: UndoHistoryCleanupPreflightRequest,
  ): Promise<HistoryCleanupUndoPreflight>;
  undoHistoryCleanup(request: UndoHistoryCleanupRequest): Promise<TimelineCleanupResult>;
  getWorkspaceGraph(request: WorkspaceGraphRequest): Promise<WorkspaceGraph>;
  getWorkspaceGraphRefs(request: WorkspaceGraphRefsRequest): Promise<WorkspaceGraphRefs>;
  getWorkspaceGraphSummary(request: WorkspaceGraphRequest): Promise<WorkspaceGraphSummary>;
  getWorkspaceGraphOverview(request: WorkspaceGraphOverviewRequest): Promise<WorkspaceGraph>;
  getWorkspaceGraphAroundVersion(
    request: WorkspaceGraphAroundVersionRequest,
  ): Promise<WorkspaceGraph>;
  getWorkspaceGraphForVariation(
    request: WorkspaceGraphVariationRequest,
  ): Promise<WorkspaceGraph>;
  getWorkspaceGraphAgentSummary(
    request: WorkspaceGraphRequest,
  ): Promise<WorkspaceGraphAgentSummary>;
  getWorkspaceGraphNeighborhood(
    request: WorkspaceGraphNeighborhoodRequest,
  ): Promise<WorkspaceGraph>;
  searchWorkspaceGraph(request: WorkspaceGraphSearchRequest): Promise<WorkspaceGraphSearchResult>;
  getWorkspaceGraphPath(request: WorkspaceGraphPairRequest): Promise<WorkspaceGraphPath>;
  getWorkspaceGraphCommonAncestor(
    request: WorkspaceGraphPairRequest,
  ): Promise<WorkspaceGraphCommonAncestor>;
  getWorkspaceGraphNode(request: VersionRequest): Promise<WorkspaceGraphNodeDetail>;
  getWorkspaceGraphCompareSummary(
    request: WorkspaceGraphPairRequest,
  ): Promise<WorkspaceGraphCompareSummary>;
  diffVersions(request: DiffVersionsRequest): Promise<VersionDiff>;
  diffVersionToWorkspace(request: VersionRequest): Promise<VersionDiff>;
  diffWorkspaceFile(request: CurrentFileRequest): Promise<CurrentFileDiff | null>;
  previewVersion(request: VersionRequest): Promise<VersionPreview>;
  previewVersionFile(request: PreviewVersionFileRequest): Promise<PreviewFile | null>;
  previewWorkspaceFile(request: CurrentFileRequest): Promise<CurrentFilePreview | null>;
  restoreVersionAsNewSave(request: RestoreVersionRequest): Promise<RestoreVersionResult>;
  restoreVersionAsNewSaveToVariation(
    request: TargetedRestoreVersionRequest,
  ): Promise<TargetedRestoreVersionResult>;
  preflightCreateVariationFromVersion(
    request: PreflightCreateVariationFromVersionRequest,
  ): Promise<VariationCreatePreflight>;
  createVariationFromVersion(
    request: CreateVariationFromVersionRequest,
  ): Promise<CreateVariationFromVersionResult>;
  createVariationFromVersionGuarded(
    request: GuardedCreateVariationFromVersionRequest,
  ): Promise<GuardedCreateVariationFromVersionResult>;
  save(request: SaveRequest): Promise<SaveResult>;
  listShelves(workspacePath: string): Promise<Shelf[]>;
  previewShelf(request: ShelfRequest): Promise<VersionPreview>;
  preflightApplyShelf(request: ShelfRequest): Promise<ShelfApplyReport>;
  applyShelf(request: ShelfRequest): Promise<ApplyShelfCommandResult>;
  deleteShelf(request: ShelfRequest): Promise<DeleteShelfResult>;
  auditContentPolicy(workspacePath: string): Promise<ContentPolicyAudit>;
  clearStaleLock(workspacePath: string): Promise<CommandPostconditions>;
  repairRecovery(request: RecoveryRequest): Promise<RecoveryRepairResult>;
  rollbackRecovery(request: RecoveryRequest): Promise<RecoveryRepairResult>;
  selectedSave(request: SelectedSaveRequest): Promise<SelectedSaveResult>;
  selectedShelve(request: SelectedShelveRequest): Promise<SelectedShelveResult>;
  selectedDiscard(request: SelectedDiscardRequest): Promise<SelectedDiscardResult>;
  publishCurrentVariation(
    request: PublishCurrentVariationRequest,
  ): Promise<PublishCurrentVariationResult>;
  fetchRemote(request: RemoteRequest): Promise<FetchRemoteResult>;
  preflightApplyIncoming(request: RemoteRequest): Promise<ApplyIncomingReport>;
  applyIncoming(request: RemoteRequest): Promise<ApplyIncomingCommandResult>;
  preflightMergeIncoming(request: RemoteRequest): Promise<MergeIncomingReport>;
  mergeIncoming(request: MergeIncomingRequest): Promise<MergeIncomingCommandResult>;
  mergeIncomingWithResolutions(
    request: MergeIncomingWithResolutionsRequest,
  ): Promise<MergeIncomingCommandResult>;
  subscribeWorkspaceEvents(handler: DraftlineEventHandler): Promise<DraftlineUnlisten>;
}

let invokeDraftline: DraftlineInvoke = (command, args) => tauriInvoke(command, args);

export function setDraftlineInvoke(invoke: DraftlineInvoke) {
  invokeDraftline = invoke;
}

export function createDraftlineClient(options: DraftlineClientOptions = {}): DraftlineClient {
  const invoke = options.invoke ?? invokeDraftline;
  const listen = options.listen ?? tauriListen;
  const workspaceEventName = options.workspaceEventName ?? 'draftline://workspace_event';

  async function run<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    options.onCommandStart?.(command, args);
    try {
      const result = await invoke<T>(command, args);
      options.onCommandSuccess?.(command, result);
      return result;
    } catch (error) {
      options.onCommandError?.(command, error);
      throw error;
    }
  }

  return {
    openWorkspace: (workspacePath) =>
      run('open_workspace', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    cloneWorkspace: (request) => run('clone_workspace', { request }),
    adoptWorkspace: (workspacePath) =>
      run('adopt_workspace', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    inspectWorkspace: (workspacePath) =>
      run('inspect_workspace', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    verifyWorkspace: (workspacePath) =>
      run('verify_workspace', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    listVariations: (workspacePath) =>
      run('list_variations', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    preflightSwitchVariation: (request) => run('preflight_switch_variation', { request }),
    switchVariation: (request) => run('switch_variation', { request }),
    preflightRenameVariation: (request) => run('preflight_rename_variation', { request }),
    renameVariation: (request) => run('rename_variation', { request }),
    listSupportRefs: (workspacePath, scope) =>
      run('list_support_refs', {
        request: { workspace_path: workspacePath, scope } satisfies ListSupportRefsRequest,
      }),
    listRemotes: (workspacePath) =>
      run('list_remotes', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    listRemoteVariations: (request) => run('list_remote_variations', { request }),
    remoteVariationDiagnostics: (request) =>
      run('remote_variation_diagnostics', { request }),
    adoptRemoteVariation: (request) => run('adopt_remote_variation', { request }),
    getChanges: (workspacePath) =>
      run('get_changes', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    getHistory: (workspacePath) =>
      run('get_history', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    getFullHistory: (workspacePath) =>
      run('get_full_history', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    previewHistoryCleanup: (request) => run('preview_history_cleanup', { request }),
    applyHistoryCleanup: (request) => run('apply_history_cleanup', { request }),
    resolveRewrittenVersion: (request) => run('resolve_rewritten_version', { request }),
    preflightUndoHistoryCleanup: (request) =>
      run('preflight_undo_history_cleanup', { request }),
    undoHistoryCleanup: (request) => run('undo_history_cleanup', { request }),
    getWorkspaceGraph: (request) => run('get_workspace_graph', { request }),
    getWorkspaceGraphRefs: (request) => run('get_workspace_graph_refs', { request }),
    getWorkspaceGraphSummary: (request) => run('get_workspace_graph_summary', { request }),
    getWorkspaceGraphOverview: (request) => run('get_workspace_graph_overview', { request }),
    getWorkspaceGraphAroundVersion: (request) =>
      run('get_workspace_graph_around_version', { request }),
    getWorkspaceGraphForVariation: (request) =>
      run('get_workspace_graph_for_variation', { request }),
    getWorkspaceGraphAgentSummary: (request) =>
      run('get_workspace_graph_agent_summary', { request }),
    getWorkspaceGraphNeighborhood: (request) =>
      run('get_workspace_graph_neighborhood', { request }),
    searchWorkspaceGraph: (request) => run('search_workspace_graph', { request }),
    getWorkspaceGraphPath: (request) => run('get_workspace_graph_path', { request }),
    getWorkspaceGraphCommonAncestor: (request) =>
      run('get_workspace_graph_common_ancestor', { request }),
    getWorkspaceGraphNode: (request) => run('get_workspace_graph_node', { request }),
    getWorkspaceGraphCompareSummary: (request) =>
      run('get_workspace_graph_compare_summary', { request }),
    diffVersions: (request) => run('diff_versions', { request }),
    diffVersionToWorkspace: (request) => run('diff_version_to_workspace', { request }),
    diffWorkspaceFile: (request) => run('diff_workspace_file', { request }),
    previewVersion: (request) => run('preview_version', { request }),
    previewVersionFile: (request) => run('preview_version_file', { request }),
    previewWorkspaceFile: (request) => run('preview_workspace_file', { request }),
    restoreVersionAsNewSave: (request) => run('restore_version_as_new_save', { request }),
    restoreVersionAsNewSaveToVariation: (request) =>
      run('restore_version_as_new_save_to_variation', { request }),
    preflightCreateVariationFromVersion: (request) =>
      run('preflight_create_variation_from_version', { request }),
    createVariationFromVersion: (request) => run('create_variation_from_version', { request }),
    createVariationFromVersionGuarded: (request) =>
      run('create_variation_from_version_guarded', { request }),
    save: (request) => run('save', { request }),
    listShelves: (workspacePath) =>
      run('list_shelves', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    previewShelf: (request) => run('preview_shelf', { request }),
    preflightApplyShelf: (request) => run('preflight_apply_shelf', { request }),
    applyShelf: (request) => run('apply_shelf', { request }),
    deleteShelf: (request) => run('delete_shelf', { request }),
    auditContentPolicy: (workspacePath) =>
      run('audit_content_policy', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    clearStaleLock: (workspacePath) =>
      run('clear_stale_lock', {
        request: { workspace_path: workspacePath } satisfies WorkspaceRequest,
      }),
    repairRecovery: (request) => run('repair_recovery', { request }),
    rollbackRecovery: (request) => run('rollback_recovery', { request }),
    selectedSave: (request) => run('selected_save', { request }),
    selectedShelve: (request) => run('selected_shelve', { request }),
    selectedDiscard: (request) => run('selected_discard', { request }),
    publishCurrentVariation: (request) => run('publish_current_variation', { request }),
    fetchRemote: (request) => run('fetch_remote', { request }),
    preflightApplyIncoming: (request) => run('preflight_apply_incoming', { request }),
    applyIncoming: (request) => run('apply_incoming', { request }),
    preflightMergeIncoming: (request) => run('preflight_merge_incoming', { request }),
    mergeIncoming: (request) => run('merge_incoming', { request }),
    mergeIncomingWithResolutions: (request) =>
      run('merge_incoming_with_resolutions', { request }),
    subscribeWorkspaceEvents: (handler) =>
      listen<DraftlineEvent>(workspaceEventName, (event) => handler(event.payload)),
  };
}

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type ChangeKind =
  | 'Added'
  | 'Modified'
  | 'Deleted'
  | 'Renamed'
  | 'Conflicted'
  | 'TypeChanged';

export type DiagnosticSeverity = 'info' | 'warning' | 'blocking';
export type OperationLockState = 'unlocked' | 'locked';
export type DraftlineEventKind =
  | 'workspace_changed'
  | 'dirty_changed'
  | 'history_changed'
  | 'sync_changed'
  | 'recovery_required'
  | 'operation_lock_changed'
  | 'policy_changed';
export type SafeNextAction =
  | 'normal_work'
  | 'save_first'
  | 'discard_changes'
  | 'repair_recovery'
  | 'configure_remote';
export type SharingMode = 'local_only' | 'shared_capable';
export type SupportRefScope = 'local' | 'remote_tracking';
export type SupportRefKind = 'deleted_variation' | 'rewrite' | 'history_cleanup_backup';
export type SyncState =
  | 'UpToDate'
  | 'LocalAhead'
  | 'IncomingAvailable'
  | 'NeedsMerge'
  | 'NoRemoteVersion';

export interface Contributor {
  name: string;
  email?: string | null;
}

export interface Version {
  id: string;
  label: string;
  author: Contributor;
  saved_by: Contributor;
  time_seconds: number;
}

export interface VariationMetadata {
  label?: string | null;
  slug?: string | null;
}

export interface Variation {
  id: string;
  name: string;
  metadata: VariationMetadata;
  is_current: boolean;
}

export type RestoreVersionTarget =
  | { kind: 'current' }
  | { kind: 'existing'; variation: string }
  | { kind: 'new'; name: string; metadata?: VariationMetadata };

export interface VariationSummary {
  variation: Variation;
  head_version?: Version | null;
  reachable_version_count: number;
}

export interface VariationRenameToken {
  operation_id: string;
  source_variation: string;
  target_variation: string;
  expected_oid: string;
  support_ref: string;
}

export interface VariationRenamePreflight {
  source_variation: string;
  target_variation: string;
  expected_oid: string;
  support_ref: string;
  token: VariationRenameToken;
  can_rename: boolean;
}

export interface SwitchVariationResult {
  preflight: PreflightReport;
  variation: Variation;
  postconditions: CommandPostconditions;
}

export interface HistoryEntry {
  version: Version;
  variation_tips: string[];
  is_head: boolean;
  parent_ids: string[];
}

export type CleanupBase = { kind: 'auto' } | { kind: 'version'; version: string };

export type CleanupMode = {
  kind: 'compact_milestones';
  milestones: MilestoneSpec[];
  preserve_named_branches: boolean;
  preserve_merge_boundaries: boolean;
};

export interface MilestoneSpec {
  title: string;
  description?: string | null;
  include_range: CommitRange;
}

export interface CommitRange {
  start: string;
  end: string;
}

export interface CleanupSafety {
  create_backup_ref: boolean;
  backup_ref_name?: string | null;
  require_clean_worktree: boolean;
}

export type RemoteRewritePolicy =
  | { kind: 'local_only' }
  | { kind: 'push_with_lease'; remote: string; branch: string };

export interface HistoryCleanupRequest {
  target_variation?: string | null;
  base: CleanupBase;
  mode: CleanupMode;
  safety: CleanupSafety;
  remote_policy: RemoteRewritePolicy;
}

export interface PreviewHistoryCleanupRequest {
  workspace_path: string;
  cleanup: HistoryCleanupRequest;
}

export interface ApplyHistoryCleanupRequest {
  workspace_path: string;
  plan_id: string;
  confirmation: RewriteConfirmation;
}

export interface ResolveRewrittenVersionRequest {
  workspace_path: string;
  version_id: string;
}

export interface UndoHistoryCleanupPreflightRequest {
  workspace_path: string;
  plan_id: string;
}

export interface UndoHistoryCleanupRequest {
  workspace_path: string;
  token: HistoryCleanupUndoToken;
}

export type RewriteConfirmation = 'user_confirmed';

export interface HistoryCleanupPreview {
  plan_id: string;
  target_variation: string;
  old_head: string;
  new_head: string;
  preview_ref: string;
  planned_backup_ref?: string | null;
  operations: CleanupOperation[];
  graph_diff: CleanupGraphDiff;
  commit_map: CommitRewriteMap[];
  snapshot_map: SnapshotRewriteMap[];
  warnings: CleanupWarning[];
}

export interface CleanupOperation {
  title: string;
  description?: string | null;
  old_versions: string[];
  new_version: string;
}

export interface CleanupGraphDiff {
  old_head: string;
  new_head: string;
  old_commit_count: number;
  new_commit_count: number;
  squashed_commit_count: number;
}

export interface CommitRewriteMap {
  old: string;
  new?: string | null;
  disposition: RewriteDisposition;
}

export interface SnapshotRewriteMap {
  old: string;
  new?: string | null;
  disposition: RewriteDisposition;
}

export type RewriteDisposition =
  | { kind: 'preserved'; new_id: string }
  | { kind: 'squashed_into'; new_id: string }
  | { kind: 'dropped_as_noise' }
  | { kind: 'orphaned_but_backed_up'; backup_ref: string }
  | { kind: 'conflict_requires_user_choice' };

export type CleanupWarningCode =
  | 'local_only_rewrite'
  | 'remote_rewrite_requires_separate_publish'
  | 'merge_boundary_requires_user_choice'
  | 'named_branch_would_be_affected'
  | 'dirty_worktree_blocked'
  | 'preview_plan_expired'
  | 'target_ref_changed_since_preview'
  | 'candidate_ref_changed_since_preview'
  | 'backup_ref_already_exists';

export interface CleanupWarning {
  code: CleanupWarningCode;
  message: string;
  related_versions: string[];
  safe_next_actions: SafeNextAction[];
}

export interface TimelineCleanupResult {
  plan_id: string;
  old_head: string;
  new_head: string;
  backup_refs: string[];
  ref_updates: RefUpdate[];
  commit_map: CommitRewriteMap[];
  snapshot_map: SnapshotRewriteMap[];
  warnings: CleanupWarning[];
}

export interface RefUpdate {
  name: string;
  old?: string | null;
  new?: string | null;
}

export interface StaleVersionResolution {
  requested: string;
  disposition: StaleVersionDisposition;
}

export type StaleVersionDisposition =
  | { kind: 'live'; version: string }
  | { kind: 'squashed_into'; version: string }
  | { kind: 'backed_up'; backup_ref: string }
  | { kind: 'dropped_as_noise' }
  | { kind: 'unknown' };

export interface HistoryCleanupUndoPreflight {
  plan_id: string;
  target_variation: string;
  backup_ref: string;
  expected_current_head: string;
  restore_head: string;
  token: HistoryCleanupUndoToken;
  can_undo: boolean;
}

export interface HistoryCleanupUndoToken {
  plan_id: string;
  target_variation: string;
  backup_ref: string;
  expected_current_head: string;
  restore_head: string;
}

export type WorkspaceGraphAction =
  | 'preview'
  | 'compare_to_current'
  | 'restore_as_new_save'
  | 'create_variation_from_here'
  | 'switch_to_variation'
  | 'adopt_remote_variation'
  | 'restore_support_ref_as_variation';

export type WorkspaceGraphNodeKind = 'normal' | 'remote_only' | 'support_ref_only';
export type WorkspaceGraphRefScope = 'local' | 'remote_tracking';
export type WorkspaceGraphRefKind = 'local_variation' | 'remote_variation' | 'support_ref';

export interface WorkspaceGraphActionHint {
  action: WorkspaceGraphAction;
  enabled: boolean;
  command: string;
  disabled_reason?: string | null;
  destructive: boolean;
  switches_workspace: boolean;
  creates_version: boolean;
}

export interface WorkspaceGraphLayoutHint {
  lane: number;
  row: number;
  group?: string | null;
  display_label: string;
}

export interface WorkspaceGraphBoundary {
  missing_parent_ids: string[];
  missing_child_ids: string[];
  hidden_parent_count: number;
  hidden_child_count: number;
}

export interface WorkspaceGraphNode {
  id: string;
  version: Version;
  parent_ids: string[];
  parent_version_ids: string[];
  variation_tips: string[];
  is_head: boolean;
  is_current: boolean;
  is_tip: boolean;
  is_merge: boolean;
  is_branch_point: boolean;
  child_ids: string[];
  child_count: number;
  kind: WorkspaceGraphNodeKind;
  topo_index: number;
  layout: WorkspaceGraphLayoutHint;
  boundary: WorkspaceGraphBoundary;
  available_actions: WorkspaceGraphAction[];
  action_hints: WorkspaceGraphActionHint[];
}

export interface WorkspaceGraphRef {
  id: string;
  name: string;
  display_label: string;
  kind: WorkspaceGraphRefKind;
  scope: WorkspaceGraphRefScope;
  target: string;
  target_version: string;
  remote?: string | null;
  variation?: string | null;
  metadata?: VariationMetadata | null;
  support_ref_kind?: SupportRefKind | null;
  group?: string | null;
  is_current: boolean;
  is_user_facing: boolean;
  available_actions: WorkspaceGraphAction[];
  action_hints: WorkspaceGraphActionHint[];
}

export interface WorkspaceGraphOptions {
  include_remotes?: boolean;
  remote?: string | null;
  include_support_refs?: boolean;
  limit?: number | null;
  cursor?: number | null;
}

export interface WorkspaceGraphRequest {
  workspace_path: string;
  options?: WorkspaceGraphOptions;
}

export type WorkspaceGraphRefsOptions = Omit<WorkspaceGraphOptions, 'limit' | 'cursor'>;

export interface WorkspaceGraphRefsRequest {
  workspace_path: string;
  options?: WorkspaceGraphRefsOptions;
}

export interface WorkspaceGraphOverviewOptions extends WorkspaceGraphOptions {
  max_nodes?: number;
  recent_nodes?: number;
}

export interface WorkspaceGraphOverviewRequest {
  workspace_path: string;
  options?: WorkspaceGraphOverviewOptions;
}

export interface WorkspaceGraphAroundVersionRequest {
  workspace_path: string;
  version_id: string;
  radius?: number;
  options?: WorkspaceGraphOptions;
}

export interface WorkspaceGraphVariationRequest {
  workspace_path: string;
  variation_id: string;
  options?: WorkspaceGraphOptions;
}

export interface WorkspaceGraphNeighborhoodRequest {
  workspace_path: string;
  version_id: string;
  radius?: number;
  options?: WorkspaceGraphOptions;
}

export interface WorkspaceGraphSearchRequest {
  workspace_path: string;
  query: string;
  options?: WorkspaceGraphOptions;
}

export interface WorkspaceGraphPairRequest {
  workspace_path: string;
  from_version_id: string;
  to_version_id: string;
  options?: WorkspaceGraphOptions;
}

export interface WorkspaceGraphRefs {
  workspace_id: { root: string };
  current_variation?: string | null;
  current_version?: string | null;
  dirty: DirtySummary;
  recovery?: RecoveryState | null;
  state_may_be_inconsistent: boolean;
  refs: WorkspaceGraphRef[];
  graph_fingerprint: string;
}

export interface WorkspaceGraphSummary {
  workspace_id: { root: string };
  current_variation?: string | null;
  current_version?: string | null;
  dirty: DirtySummary;
  recovery?: RecoveryState | null;
  state_may_be_inconsistent: boolean;
  total_nodes: number;
  normal_nodes: number;
  remote_only_nodes: number;
  support_ref_only_nodes: number;
  merge_nodes: number;
  branch_points: number;
  local_ref_count: number;
  remote_ref_count: number;
  support_ref_count: number;
  graph_fingerprint: string;
}

export interface WorkspaceGraphAgentSummary {
  summary: WorkspaceGraphSummary;
  suggested_next_commands: string[];
  warnings: string[];
  current_ref?: WorkspaceGraphRef | null;
  nearby_refs: WorkspaceGraphRef[];
}

export interface WorkspaceGraphSearchResult {
  graph: WorkspaceGraph;
  matched_refs: WorkspaceGraphRef[];
  query: string;
  matched_node_count: number;
  total_matches: number;
}

export interface WorkspaceGraphPath {
  from_version: string;
  to_version: string;
  node_ids: string[];
  version_ids: string[];
  common_ancestor?: string | null;
  found: boolean;
}

export interface WorkspaceGraphCommonAncestor {
  left_version: string;
  right_version: string;
  common_ancestor?: string | null;
}

export interface WorkspaceGraphNodeDetail {
  node: WorkspaceGraphNode;
  refs: WorkspaceGraphRef[];
  changed_file_count?: number | null;
  changed_files: ChangedFile[];
}

export interface WorkspaceGraphCompareSummary {
  from_version: string;
  to_version: string;
  changed_file_count: number;
  files: ChangedFile[];
  action_hints: WorkspaceGraphActionHint[];
  common_ancestor?: string | null;
}

export interface WorkspaceGraph {
  workspace_id: { root: string };
  current_variation?: string | null;
  current_version?: string | null;
  dirty: DirtySummary;
  recovery?: RecoveryState | null;
  state_may_be_inconsistent: boolean;
  nodes: WorkspaceGraphNode[];
  refs: WorkspaceGraphRef[];
  snapshot_id: string;
  was_pruned: boolean;
  has_more: boolean;
  next_cursor?: number | null;
}

export interface ChangedFile {
  path: string;
  kind: ChangeKind;
  is_binary: boolean;
  is_large: boolean;
}

export interface ChangeSet {
  files: ChangedFile[];
  diff?: string | null;
}

export interface VersionDiff {
  from_version?: string | null;
  to_version?: string | null;
  files: ChangedFile[];
  patch?: string | null;
}

export interface CurrentFileDiff {
  path: string;
  file?: ChangedFile | null;
  patch?: string | null;
  preview?: CurrentFilePreview | null;
}

export interface PreviewFile {
  path: string;
  content?: string | null;
  is_binary: boolean;
}

export interface CurrentFilePreview {
  path: string;
  content?: string | null;
  is_binary: boolean;
}

export interface VersionPreview {
  id: string;
  files: PreviewFile[];
}

export interface FileHazard {
  path: string;
  kind: string;
}

export interface PreflightReport {
  operation: string;
  will_write_files: boolean;
  dirty_files: ChangedFile[];
  file_hazards: FileHazard[];
  untracked_assets: string[];
  unresolved_conflicts: string[];
  large_files: string[];
  binary_files: string[];
  variation_divergence?: string | null;
  can_proceed: boolean;
}

export interface RecoveryState {
  operation_id: string;
  operation: string;
  original_variation?: string | null;
  target?: string | null;
  completed: boolean;
}

export interface WorkspaceSummary {
  active_variation: Variation;
  variations: Variation[];
  versions: Version[];
  dirty_files: ChangedFile[];
  is_dirty: boolean;
  recovery?: RecoveryState | null;
  state_may_be_inconsistent: boolean;
}

export interface RemoteEndpoint {
  name: string;
  url: string;
}

export interface RemoteVariation {
  id: string;
  name: string;
  remote: string;
  head_version?: Version | null;
}

export interface RemoteVariationDiagnostics {
  remote: string;
  shared_variations: string[];
  local_only_variations: string[];
  remote_only_variations: string[];
}

export interface VariationCreateToken {
  operation_id: string;
  from_version: string;
  variation: string;
  remote?: string | null;
  expected_source_oid: string;
  expected_remote_oid?: string | null;
}

export interface VariationCreatePreflight {
  from_version: string;
  variation: string;
  remote?: string | null;
  can_create: boolean;
  local_collision: boolean;
  remote_collision: boolean;
  remote_only_collision: boolean;
  existing_remote_head?: Version | null;
  suggested_alternative?: string | null;
  token?: VariationCreateToken | null;
}

export interface DirtySummary {
  is_dirty: boolean;
  files: ChangedFile[];
}

export interface OperationLockSummary {
  state: OperationLockState;
}

export interface OperationLockMetadata {
  operation_id: string;
  operation: string;
  process_id: number;
  owner?: string | null;
  created_at_seconds: number;
}

export interface WorkspaceDiagnostic {
  code: string;
  severity: DiagnosticSeverity;
  message: string;
}

export interface OperationLockInspection {
  state: OperationLockState;
  metadata?: OperationLockMetadata | null;
  is_stale: boolean;
  can_clear: boolean;
  diagnostics: WorkspaceDiagnostic[];
}

export interface SupportRefSummary {
  local_count: number;
  remote_count: number;
}

export interface WorkspaceInspection {
  workspace_id: { root: string };
  sharing_mode: SharingMode;
  current_variation?: string | null;
  remotes: RemoteEndpoint[];
  dirty: DirtySummary;
  recovery?: RecoveryState | null;
  operation_lock: OperationLockSummary;
  support_refs: SupportRefSummary;
  diagnostics: WorkspaceDiagnostic[];
  safe_next_actions: SafeNextAction[];
}

export interface WorkspaceVerification {
  recovery_clear: boolean;
  operation_lock_clear: boolean;
  current_variation_present: boolean;
  diagnostics: WorkspaceDiagnostic[];
}

export interface WorkspaceDiagnostics {
  summary: WorkspaceSummary;
  inspection: WorkspaceInspection;
  verification: WorkspaceVerification;
  operation_lock: OperationLockInspection;
}

export interface WorkspaceOpenResult {
  diagnostics: WorkspaceDiagnostics;
}

export interface AdoptWorkspaceResult {
  preflight: AdoptionPreflightReport;
  diagnostics: WorkspaceDiagnostics;
}

export interface AdoptionPreflightReport {
  inspection: WorkspaceInspection;
  candidate_policy_diagnostics: WorkspaceDiagnostic[];
  blockers: WorkspaceDiagnostic[];
  warnings: WorkspaceDiagnostic[];
  safe_next_actions: SafeNextAction[];
  can_adopt: boolean;
}

export interface ContentPolicyAudit {
  current_diagnostics: WorkspaceDiagnostic[];
  historical_out_of_policy_paths: string[];
}

export interface DraftlineEvent {
  workspace_id: { root: string };
  kind: DraftlineEventKind;
  sequence: number;
  changed_paths: string[];
  active_variation?: string | null;
  dirty?: DirtySummary | null;
  sync?: SyncStatus | null;
  recovery?: RecoveryState | null;
  diagnostics: WorkspaceDiagnostic[];
}

export interface TauriCommandError {
  code: string;
  message: string;
  details?: JsonValue;
}

export interface CommandPostconditions {
  remaining_changes?: ChangeSet | null;
  verification?: WorkspaceVerification | null;
  errors: TauriCommandError[];
}

export interface Shelf {
  id: string;
  version: Version;
}

export interface ShelfApplyReport {
  shelf: Shelf;
  dirty_files: ChangedFile[];
  file_hazards: FileHazard[];
  can_proceed: boolean;
}

export interface RestoreVersionResult {
  version: Version;
  postconditions: CommandPostconditions;
}

export interface TargetedRestoreVersionResult {
  version: Version;
  target_variation: Variation;
  postconditions: CommandPostconditions;
}

export interface CreateVariationFromVersionResult {
  variation: Variation;
  postconditions: CommandPostconditions;
}

export interface GuardedCreateVariationFromVersionResult {
  preflight: VariationCreatePreflight;
  variation: Variation;
  postconditions: CommandPostconditions;
}

export interface SaveResult {
  version: Version;
  postconditions: CommandPostconditions;
}

export interface ApplyShelfCommandResult {
  preflight: ShelfApplyReport;
  shelf: Shelf;
  postconditions: CommandPostconditions;
}

export interface DeleteShelfResult {
  postconditions: CommandPostconditions;
}

export interface RecoveryRepairResult {
  operation_id: string;
  operation: string;
  completed: boolean;
  changed_workspace: boolean;
  safe_next_actions: SafeNextAction[];
}

export interface SelectedSaveResult {
  preflight: PreflightReport;
  version: Version;
  postconditions: CommandPostconditions;
}

export interface SelectedShelveResult {
  preflight: PreflightReport;
  shelf: Shelf;
  postconditions: CommandPostconditions;
}

export interface SelectedDiscardResult {
  preflight: PreflightReport;
  discarded: ChangeSet;
  postconditions: CommandPostconditions;
}

export interface RemoteVersionSummary {
  id: string;
  label: string;
  author: Contributor;
  time_seconds: number;
}

export interface SyncStatus {
  remote: string;
  variation: string;
  ahead: number;
  behind: number;
  state: SyncState;
  incoming: RemoteVersionSummary[];
}

export interface PublishPreflight {
  remote: string;
  variation: string;
  expected_remote_oid?: string | null;
  local_oid: string;
  sync_status: SyncStatus;
  token: JsonValue;
  can_publish: boolean;
}

export interface PublishResult {
  remote: string;
  variation: string;
  published_versions: number;
}

export interface PublishCurrentVariationResult {
  preflight: PublishPreflight;
  publish: PublishResult;
  postconditions: CommandPostconditions;
}

export interface AdoptRemoteVariationResult {
  variation: Variation;
  postconditions: CommandPostconditions;
}

export interface RenameVariationResult {
  preflight: VariationRenamePreflight;
  variation: Variation;
  postconditions: CommandPostconditions;
}

export interface ApplyIncomingReport {
  sync_status: SyncStatus;
  dirty_files: ChangedFile[];
  file_hazards: FileHazard[];
  is_fast_forward: boolean;
  can_proceed: boolean;
}

export interface ApplyIncomingResult {
  applied_count: number;
}

export interface ApplyIncomingCommandResult {
  preflight: ApplyIncomingReport;
  apply: ApplyIncomingResult;
  postconditions: CommandPostconditions;
}

export interface MergeConflict {
  path: string;
  field_path?: string | null;
  label: string;
  base?: string | null;
  ours?: string | null;
  theirs?: string | null;
  resolution: 'Choose' | 'Edit' | 'Combine' | 'Delete';
}

export type MergeResolutionChoice =
  | { kind: 'use_ours' }
  | { kind: 'use_theirs' }
  | { kind: 'use_base' }
  | { kind: 'delete' }
  | { kind: 'use_content'; content: string };

export interface MergeConflictResolution {
  path: string;
  field_path?: string | null;
  choice: MergeResolutionChoice;
}

export interface MergeIncomingToken {
  remote: string;
  variation: string;
  local_oid: string;
  remote_oid: string;
  merge_base_oid: string;
}

export interface MergeIncomingReport {
  sync_status: SyncStatus;
  dirty_files: ChangedFile[];
  file_hazards: FileHazard[];
  conflicts: MergeConflict[];
  token?: MergeIncomingToken | null;
  can_merge_cleanly: boolean;
  changed_workspace: boolean;
}

export interface MergeIncomingResult {
  version: Version;
  merged_files: string[];
}

export interface MergeIncomingCommandResult {
  preflight: MergeIncomingReport;
  merge: MergeIncomingResult;
  postconditions: CommandPostconditions;
}

export type ConflictContentSource = 'ours' | 'theirs' | 'base';

export interface MergeConflictViewModel {
  files: MergeFileConflictGroup[];
  token?: MergeIncomingToken | null;
  can_merge_cleanly: boolean;
}

export interface MergeFileConflictGroup {
  path: string;
  label: string;
  whole_file_conflicts: MergeConflictItem[];
  field_conflicts: MergeFieldConflictGroup[];
}

export interface MergeFieldConflictGroup {
  field_path: string;
  label: string;
  conflicts: MergeConflictItem[];
}

export interface MergeConflictItem extends MergeConflict {}

export interface SupportRef {
  id: string;
  ref_name: string;
  kind: SupportRefKind;
  source_variation?: string | null;
  target_oid: string;
  scope: SupportRefScope;
}

export interface WorkspaceRequest {
  workspace_path: string;
}

export interface CloneWorkspaceRequest extends WorkspaceRequest {
  remote_url: string;
}

export interface ListSupportRefsRequest extends WorkspaceRequest {
  scope: SupportRefScope;
}

export interface RenameVariationRequest extends WorkspaceRequest {
  source_variation_id: string;
  target_variation_id: string;
  token?: VariationRenameToken;
}

export interface SwitchVariationRequest extends WorkspaceRequest {
  variation_id: string;
}

export interface VersionRequest extends WorkspaceRequest {
  version_id: string;
}

export interface CurrentFileRequest extends WorkspaceRequest {
  path: string;
}

export interface PreviewVersionFileRequest extends VersionRequest {
  path: string;
}

export interface DiffVersionsRequest extends WorkspaceRequest {
  from_version_id: string;
  to_version_id: string;
}

export interface RestoreVersionRequest extends VersionRequest {
  label: string;
}

export interface TargetedRestoreVersionRequest extends RestoreVersionRequest {
  target: RestoreVersionTarget;
}

export interface CreateVariationFromVersionRequest extends WorkspaceRequest {
  version_id: string;
  name: string;
  metadata?: VariationMetadata;
}

export interface PreflightCreateVariationFromVersionRequest extends WorkspaceRequest {
  version_id: string;
  name: string;
  remote?: string | null;
}

export interface GuardedCreateVariationFromVersionRequest extends WorkspaceRequest {
  token: VariationCreateToken;
  metadata?: VariationMetadata;
}

export interface SaveRequest extends WorkspaceRequest {
  label: string;
}

export interface ShelfRequest extends WorkspaceRequest {
  shelf_id: string;
}

export interface RecoveryRequest extends WorkspaceRequest {
  operation_id: string;
}

export interface SelectedSaveRequest extends WorkspaceRequest {
  paths: string[];
  label: string;
}

export interface SelectedShelveRequest extends WorkspaceRequest {
  paths: string[];
  name: string;
}

export interface SelectedDiscardRequest extends WorkspaceRequest {
  paths: string[];
}

export interface PublishCurrentVariationRequest extends WorkspaceRequest {
  remote: string;
}

export interface RemoteRequest extends WorkspaceRequest {
  remote: string;
}

export interface RemoteVariationRequest extends RemoteRequest {
  variation_id: string;
}

export interface FetchRemoteResult {
  sync_status: SyncStatus;
  postconditions: CommandPostconditions;
}

export interface MergeIncomingRequest extends RemoteRequest {
  label: string;
}

export interface MergeIncomingWithResolutionsRequest extends MergeIncomingRequest {
  token: MergeIncomingToken;
  resolutions: MergeConflictResolution[];
}

export interface DraftlineHostFacadeOptions {
  client?: DraftlineClient;
  defaultRemote?: string;
  workspacePath: string;
}

export interface DraftlineHostFacade {
  workspacePath: string;
  open(): Promise<WorkspaceOpenResult>;
  inspect(): Promise<WorkspaceDiagnostics>;
  preflightSwitchVariation(variationId: string): Promise<PreflightReport>;
  switchVariation(variationId: string): Promise<SwitchVariationResult>;
  save(label: string): Promise<SaveResult>;
  selectedSave(paths: string[], label: string): Promise<SelectedSaveResult>;
  selectedShelve(paths: string[], name: string): Promise<SelectedShelveResult>;
  selectedDiscard(paths: string[]): Promise<SelectedDiscardResult>;
  history(): Promise<HistoryEntry[]>;
  fullHistory(): Promise<HistoryEntry[]>;
  workspaceGraph(options?: WorkspaceGraphOptions): Promise<WorkspaceGraph>;
  workspaceGraphRefs(options?: WorkspaceGraphRefsOptions): Promise<WorkspaceGraphRefs>;
  workspaceGraphSummary(options?: WorkspaceGraphOptions): Promise<WorkspaceGraphSummary>;
  workspaceGraphOverview(options?: WorkspaceGraphOverviewOptions): Promise<WorkspaceGraph>;
  workspaceGraphAroundVersion(
    versionId: string,
    radius?: number,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraph>;
  workspaceGraphForVariation(
    variationId: string,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraph>;
  workspaceGraphAgentSummary(options?: WorkspaceGraphOptions): Promise<WorkspaceGraphAgentSummary>;
  workspaceGraphNeighborhood(
    versionId: string,
    radius?: number,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraph>;
  searchWorkspaceGraph(
    query: string,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraphSearchResult>;
  workspaceGraphPath(
    fromVersionId: string,
    toVersionId: string,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraphPath>;
  workspaceGraphCommonAncestor(
    fromVersionId: string,
    toVersionId: string,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraphCommonAncestor>;
  workspaceGraphNode(versionId: string): Promise<WorkspaceGraphNodeDetail>;
  workspaceGraphCompareSummary(
    fromVersionId: string,
    toVersionId: string,
    options?: WorkspaceGraphOptions,
  ): Promise<WorkspaceGraphCompareSummary>;
  changes(): Promise<ChangeSet>;
  variations(): Promise<VariationSummary[]>;
  preflightRenameVariation(sourceVariationId: string, targetVariationId: string): Promise<VariationRenamePreflight>;
  renameVariation(
    sourceVariationId: string,
    targetVariationId: string,
    token?: VariationRenameToken,
  ): Promise<RenameVariationResult>;
  remotes(): Promise<RemoteEndpoint[]>;
  supportRefs(scope: SupportRefScope): Promise<SupportRef[]>;
  diffVersions(fromVersionId: string, toVersionId: string): Promise<VersionDiff>;
  diffVersionToWorkspace(versionId: string): Promise<VersionDiff>;
  diffWorkspaceFile(path: string): Promise<CurrentFileDiff | null>;
  previewVersion(versionId: string): Promise<VersionPreview>;
  previewVersionFile(versionId: string, path: string): Promise<PreviewFile | null>;
  previewWorkspaceFile(path: string): Promise<CurrentFilePreview | null>;
  restoreAsNewSave(versionId: string, label: string): Promise<RestoreVersionResult>;
  restoreAsNewSaveToVariation(
    versionId: string,
    label: string,
    target: RestoreVersionTarget,
  ): Promise<TargetedRestoreVersionResult>;
  createVariationFromVersion(
    versionId: string,
    name: string,
    metadata?: VariationMetadata,
  ): Promise<CreateVariationFromVersionResult>;
  preflightCreateVariationFromVersion(
    versionId: string,
    name: string,
    remote?: string,
  ): Promise<VariationCreatePreflight>;
  createVariationFromVersionGuarded(
    token: VariationCreateToken,
    metadata?: VariationMetadata,
  ): Promise<GuardedCreateVariationFromVersionResult>;
  shelves(): Promise<Shelf[]>;
  previewShelf(shelfId: string): Promise<VersionPreview>;
  applyShelf(shelfId: string): Promise<ApplyShelfCommandResult>;
  repairRecovery(operationId: string): Promise<RecoveryRepairResult>;
  rollbackRecovery(operationId: string): Promise<RecoveryRepairResult>;
  fetchRemote(remote?: string): Promise<FetchRemoteResult>;
  publishCurrentVariation(remote?: string): Promise<PublishCurrentVariationResult>;
  preflightApplyIncoming(remote?: string): Promise<ApplyIncomingReport>;
  applyIncoming(remote?: string): Promise<ApplyIncomingCommandResult>;
  preflightMergeIncoming(remote?: string): Promise<MergeIncomingReport>;
  mergeIncoming(label: string, remote?: string): Promise<MergeIncomingCommandResult>;
  mergeIncomingWithResolutions(
    label: string,
    token: MergeIncomingToken,
    resolutions: MergeConflictResolution[],
    remote?: string,
  ): Promise<MergeIncomingCommandResult>;
  remoteVariations(remote?: string): Promise<RemoteVariation[]>;
  remoteVariationDiagnostics(remote?: string): Promise<RemoteVariationDiagnostics>;
  adoptRemoteVariation(variationId: string, remote?: string): Promise<AdoptRemoteVariationResult>;
}

export function createDraftlineHostFacade({
  client = createDraftlineClient(),
  defaultRemote = 'origin',
  workspacePath,
}: DraftlineHostFacadeOptions): DraftlineHostFacade {
  const workspaceRequest = () => ({ workspace_path: workspacePath });
  const remoteRequest = (remote = defaultRemote) => ({ ...workspaceRequest(), remote });
  const versionRequest = (versionId: string) => ({
    ...workspaceRequest(),
    version_id: versionId,
  });

  return {
    workspacePath,
    open: () => client.openWorkspace(workspacePath),
    inspect: () => client.inspectWorkspace(workspacePath),
    preflightSwitchVariation: (variationId) =>
      client.preflightSwitchVariation({ ...workspaceRequest(), variation_id: variationId }),
    switchVariation: (variationId) =>
      client.switchVariation({ ...workspaceRequest(), variation_id: variationId }),
    save: (label) => client.save({ ...workspaceRequest(), label }),
    selectedSave: (paths, label) => client.selectedSave({ ...workspaceRequest(), paths, label }),
    selectedShelve: (paths, name) => client.selectedShelve({ ...workspaceRequest(), paths, name }),
    selectedDiscard: (paths) => client.selectedDiscard({ ...workspaceRequest(), paths }),
    history: () => client.getHistory(workspacePath),
    fullHistory: () => client.getFullHistory(workspacePath),
    workspaceGraph: (options) => client.getWorkspaceGraph({ ...workspaceRequest(), options }),
    workspaceGraphRefs: (options) =>
      client.getWorkspaceGraphRefs({ ...workspaceRequest(), options }),
    workspaceGraphSummary: (options) =>
      client.getWorkspaceGraphSummary({ ...workspaceRequest(), options }),
    workspaceGraphOverview: (options) =>
      client.getWorkspaceGraphOverview({ ...workspaceRequest(), options }),
    workspaceGraphAroundVersion: (versionId, radius, options) =>
      client.getWorkspaceGraphAroundVersion({
        ...workspaceRequest(),
        version_id: versionId,
        radius,
        options,
      }),
    workspaceGraphForVariation: (variationId, options) =>
      client.getWorkspaceGraphForVariation({
        ...workspaceRequest(),
        variation_id: variationId,
        options,
      }),
    workspaceGraphAgentSummary: (options) =>
      client.getWorkspaceGraphAgentSummary({ ...workspaceRequest(), options }),
    workspaceGraphNeighborhood: (versionId, radius, options) =>
      client.getWorkspaceGraphNeighborhood({
        ...workspaceRequest(),
        version_id: versionId,
        radius,
        options,
      }),
    searchWorkspaceGraph: (query, options) =>
      client.searchWorkspaceGraph({ ...workspaceRequest(), query, options }),
    workspaceGraphPath: (fromVersionId, toVersionId, options) =>
      client.getWorkspaceGraphPath({
        ...workspaceRequest(),
        from_version_id: fromVersionId,
        to_version_id: toVersionId,
        options,
      }),
    workspaceGraphCommonAncestor: (fromVersionId, toVersionId, options) =>
      client.getWorkspaceGraphCommonAncestor({
        ...workspaceRequest(),
        from_version_id: fromVersionId,
        to_version_id: toVersionId,
        options,
      }),
    workspaceGraphNode: (versionId) => client.getWorkspaceGraphNode(versionRequest(versionId)),
    workspaceGraphCompareSummary: (fromVersionId, toVersionId, options) =>
      client.getWorkspaceGraphCompareSummary({
        ...workspaceRequest(),
        from_version_id: fromVersionId,
        to_version_id: toVersionId,
        options,
      }),
    changes: () => client.getChanges(workspacePath),
    variations: () => client.listVariations(workspacePath),
    preflightRenameVariation: (sourceVariationId, targetVariationId) =>
      client.preflightRenameVariation({
        ...workspaceRequest(),
        source_variation_id: sourceVariationId,
        target_variation_id: targetVariationId,
      }),
    renameVariation: (sourceVariationId, targetVariationId, token) =>
      client.renameVariation({
        ...workspaceRequest(),
        source_variation_id: sourceVariationId,
        target_variation_id: targetVariationId,
        token,
      }),
    remotes: () => client.listRemotes(workspacePath),
    supportRefs: (scope) => client.listSupportRefs(workspacePath, scope),
    diffVersions: (fromVersionId, toVersionId) =>
      client.diffVersions({
        ...workspaceRequest(),
        from_version_id: fromVersionId,
        to_version_id: toVersionId,
      }),
    diffVersionToWorkspace: (versionId) => client.diffVersionToWorkspace(versionRequest(versionId)),
    diffWorkspaceFile: (path) => client.diffWorkspaceFile({ ...workspaceRequest(), path }),
    previewVersion: (versionId) => client.previewVersion(versionRequest(versionId)),
    previewVersionFile: (versionId, path) =>
      client.previewVersionFile({ ...versionRequest(versionId), path }),
    previewWorkspaceFile: (path) => client.previewWorkspaceFile({ ...workspaceRequest(), path }),
    restoreAsNewSave: (versionId, label) =>
      client.restoreVersionAsNewSave({ ...versionRequest(versionId), label }),
    restoreAsNewSaveToVariation: (versionId, label, target) =>
      client.restoreVersionAsNewSaveToVariation({ ...versionRequest(versionId), label, target }),
    createVariationFromVersion: (versionId, name, metadata) =>
      client.createVariationFromVersion({ ...versionRequest(versionId), name, metadata }),
    preflightCreateVariationFromVersion: (versionId, name, remote = defaultRemote) =>
      client.preflightCreateVariationFromVersion({ ...versionRequest(versionId), name, remote }),
    createVariationFromVersionGuarded: (token, metadata) =>
      client.createVariationFromVersionGuarded({ ...workspaceRequest(), token, metadata }),
    shelves: () => client.listShelves(workspacePath),
    previewShelf: (shelfId) => client.previewShelf({ ...workspaceRequest(), shelf_id: shelfId }),
    applyShelf: (shelfId) => client.applyShelf({ ...workspaceRequest(), shelf_id: shelfId }),
    repairRecovery: (operationId) =>
      client.repairRecovery({ ...workspaceRequest(), operation_id: operationId }),
    rollbackRecovery: (operationId) =>
      client.rollbackRecovery({ ...workspaceRequest(), operation_id: operationId }),
    fetchRemote: (remote) => client.fetchRemote(remoteRequest(remote)),
    publishCurrentVariation: (remote) => client.publishCurrentVariation(remoteRequest(remote)),
    preflightApplyIncoming: (remote) => client.preflightApplyIncoming(remoteRequest(remote)),
    applyIncoming: (remote) => client.applyIncoming(remoteRequest(remote)),
    preflightMergeIncoming: (remote) => client.preflightMergeIncoming(remoteRequest(remote)),
    mergeIncoming: (label, remote) => client.mergeIncoming({ ...remoteRequest(remote), label }),
    mergeIncomingWithResolutions: (label, token, resolutions, remote) =>
      client.mergeIncomingWithResolutions({
        ...remoteRequest(remote),
        label,
        resolutions,
        token,
      }),
    remoteVariations: (remote) => client.listRemoteVariations(remoteRequest(remote)),
    remoteVariationDiagnostics: (remote) => client.remoteVariationDiagnostics(remoteRequest(remote)),
    adoptRemoteVariation: (variationId, remote) =>
      client.adoptRemoteVariation({
        ...remoteRequest(remote),
        variation_id: variationId,
      }),
  };
}

export function createMergeConflictViewModel(
  report: MergeIncomingReport,
): MergeConflictViewModel {
  const files = new Map<string, MergeFileConflictGroup>();
  for (const conflict of report.conflicts) {
    const group =
      files.get(conflict.path) ??
      {
        field_conflicts: [],
        label: conflict.path,
        path: conflict.path,
        whole_file_conflicts: [],
      };
    files.set(conflict.path, group);

    if (conflict.field_path) {
      let field = group.field_conflicts.find(
        (candidate) => candidate.field_path === conflict.field_path,
      );
      if (!field) {
        field = {
          conflicts: [],
          field_path: conflict.field_path,
          label: conflict.label,
        };
        group.field_conflicts.push(field);
      }
      field.conflicts.push({ ...conflict });
    } else {
      group.whole_file_conflicts.push({ ...conflict });
    }
  }

  return {
    can_merge_cleanly: report.can_merge_cleanly,
    files: [...files.values()].sort((left, right) => left.path.localeCompare(right.path)),
    token: report.token,
  };
}

export function createWholeFileUseContentResolutions(
  report: MergeIncomingReport,
  source: ConflictContentSource,
): MergeConflictResolution[] {
  return report.conflicts.flatMap((conflict) => {
    if (conflict.field_path) {
      return [];
    }
    const content = conflict[source];
    if (content == null) {
      return [];
    }
    return [
      {
        choice: { kind: 'use_content', content },
        field_path: null,
        path: conflict.path,
      },
    ];
  });
}

export async function openWorkspace(workspacePath: string): Promise<WorkspaceOpenResult> {
  return createDraftlineClient().openWorkspace(workspacePath);
}

export async function cloneWorkspace(
  request: CloneWorkspaceRequest,
): Promise<WorkspaceOpenResult> {
  return createDraftlineClient().cloneWorkspace(request);
}

export async function adoptWorkspace(workspacePath: string): Promise<AdoptWorkspaceResult> {
  return createDraftlineClient().adoptWorkspace(workspacePath);
}

export async function inspectWorkspace(workspacePath: string): Promise<WorkspaceDiagnostics> {
  return createDraftlineClient().inspectWorkspace(workspacePath);
}

export async function verifyWorkspace(workspacePath: string): Promise<WorkspaceVerification> {
  return createDraftlineClient().verifyWorkspace(workspacePath);
}

export async function listVariations(workspacePath: string): Promise<VariationSummary[]> {
  return createDraftlineClient().listVariations(workspacePath);
}

export async function preflightRenameVariation(
  request: RenameVariationRequest,
): Promise<VariationRenamePreflight> {
  return createDraftlineClient().preflightRenameVariation(request);
}

export async function renameVariation(
  request: RenameVariationRequest,
): Promise<RenameVariationResult> {
  return createDraftlineClient().renameVariation(request);
}

export async function listSupportRefs(
  workspacePath: string,
  scope: SupportRefScope,
): Promise<SupportRef[]> {
  return createDraftlineClient().listSupportRefs(workspacePath, scope);
}

export async function listRemotes(workspacePath: string): Promise<RemoteEndpoint[]> {
  return createDraftlineClient().listRemotes(workspacePath);
}

export async function listRemoteVariations(request: RemoteRequest): Promise<RemoteVariation[]> {
  return createDraftlineClient().listRemoteVariations(request);
}

export async function preflightSwitchVariation(
  request: SwitchVariationRequest,
): Promise<PreflightReport> {
  return createDraftlineClient().preflightSwitchVariation(request);
}

export async function switchVariation(
  request: SwitchVariationRequest,
): Promise<SwitchVariationResult> {
  return createDraftlineClient().switchVariation(request);
}

export async function remoteVariationDiagnostics(
  request: RemoteRequest,
): Promise<RemoteVariationDiagnostics> {
  return createDraftlineClient().remoteVariationDiagnostics(request);
}

export async function adoptRemoteVariation(
  request: RemoteVariationRequest,
): Promise<AdoptRemoteVariationResult> {
  return createDraftlineClient().adoptRemoteVariation(request);
}

export async function getChanges(workspacePath: string): Promise<ChangeSet> {
  return createDraftlineClient().getChanges(workspacePath);
}

export async function getHistory(workspacePath: string): Promise<HistoryEntry[]> {
  return createDraftlineClient().getHistory(workspacePath);
}

export async function getFullHistory(workspacePath: string): Promise<HistoryEntry[]> {
  return createDraftlineClient().getFullHistory(workspacePath);
}

export async function getWorkspaceGraph(request: WorkspaceGraphRequest): Promise<WorkspaceGraph> {
  return createDraftlineClient().getWorkspaceGraph(request);
}

export async function getWorkspaceGraphRefs(
  request: WorkspaceGraphRefsRequest,
): Promise<WorkspaceGraphRefs> {
  return createDraftlineClient().getWorkspaceGraphRefs(request);
}

export async function getWorkspaceGraphSummary(
  request: WorkspaceGraphRequest,
): Promise<WorkspaceGraphSummary> {
  return createDraftlineClient().getWorkspaceGraphSummary(request);
}

export async function getWorkspaceGraphOverview(
  request: WorkspaceGraphOverviewRequest,
): Promise<WorkspaceGraph> {
  return createDraftlineClient().getWorkspaceGraphOverview(request);
}

export async function getWorkspaceGraphAroundVersion(
  request: WorkspaceGraphAroundVersionRequest,
): Promise<WorkspaceGraph> {
  return createDraftlineClient().getWorkspaceGraphAroundVersion(request);
}

export async function getWorkspaceGraphForVariation(
  request: WorkspaceGraphVariationRequest,
): Promise<WorkspaceGraph> {
  return createDraftlineClient().getWorkspaceGraphForVariation(request);
}

export async function getWorkspaceGraphAgentSummary(
  request: WorkspaceGraphRequest,
): Promise<WorkspaceGraphAgentSummary> {
  return createDraftlineClient().getWorkspaceGraphAgentSummary(request);
}

export async function getWorkspaceGraphNeighborhood(
  request: WorkspaceGraphNeighborhoodRequest,
): Promise<WorkspaceGraph> {
  return createDraftlineClient().getWorkspaceGraphNeighborhood(request);
}

export async function searchWorkspaceGraph(
  request: WorkspaceGraphSearchRequest,
): Promise<WorkspaceGraphSearchResult> {
  return createDraftlineClient().searchWorkspaceGraph(request);
}

export async function getWorkspaceGraphPath(
  request: WorkspaceGraphPairRequest,
): Promise<WorkspaceGraphPath> {
  return createDraftlineClient().getWorkspaceGraphPath(request);
}

export async function getWorkspaceGraphCommonAncestor(
  request: WorkspaceGraphPairRequest,
): Promise<WorkspaceGraphCommonAncestor> {
  return createDraftlineClient().getWorkspaceGraphCommonAncestor(request);
}

export async function getWorkspaceGraphNode(
  request: VersionRequest,
): Promise<WorkspaceGraphNodeDetail> {
  return createDraftlineClient().getWorkspaceGraphNode(request);
}

export async function getWorkspaceGraphCompareSummary(
  request: WorkspaceGraphPairRequest,
): Promise<WorkspaceGraphCompareSummary> {
  return createDraftlineClient().getWorkspaceGraphCompareSummary(request);
}

export async function diffVersions(request: DiffVersionsRequest): Promise<VersionDiff> {
  return createDraftlineClient().diffVersions(request);
}

export async function diffVersionToWorkspace(request: VersionRequest): Promise<VersionDiff> {
  return createDraftlineClient().diffVersionToWorkspace(request);
}

export async function diffWorkspaceFile(
  request: CurrentFileRequest,
): Promise<CurrentFileDiff | null> {
  return createDraftlineClient().diffWorkspaceFile(request);
}

export async function previewVersion(request: VersionRequest): Promise<VersionPreview> {
  return createDraftlineClient().previewVersion(request);
}

export async function previewVersionFile(
  request: PreviewVersionFileRequest,
): Promise<PreviewFile | null> {
  return createDraftlineClient().previewVersionFile(request);
}

export async function previewWorkspaceFile(
  request: CurrentFileRequest,
): Promise<CurrentFilePreview | null> {
  return createDraftlineClient().previewWorkspaceFile(request);
}

export async function restoreVersionAsNewSave(
  request: RestoreVersionRequest,
): Promise<RestoreVersionResult> {
  return createDraftlineClient().restoreVersionAsNewSave(request);
}

export async function restoreVersionAsNewSaveToVariation(
  request: TargetedRestoreVersionRequest,
): Promise<TargetedRestoreVersionResult> {
  return createDraftlineClient().restoreVersionAsNewSaveToVariation(request);
}

export async function preflightCreateVariationFromVersion(
  request: PreflightCreateVariationFromVersionRequest,
): Promise<VariationCreatePreflight> {
  return createDraftlineClient().preflightCreateVariationFromVersion(request);
}

export async function createVariationFromVersion(
  request: CreateVariationFromVersionRequest,
): Promise<CreateVariationFromVersionResult> {
  return createDraftlineClient().createVariationFromVersion(request);
}

export async function createVariationFromVersionGuarded(
  request: GuardedCreateVariationFromVersionRequest,
): Promise<GuardedCreateVariationFromVersionResult> {
  return createDraftlineClient().createVariationFromVersionGuarded(request);
}

export async function save(request: SaveRequest): Promise<SaveResult> {
  return createDraftlineClient().save(request);
}

export async function listShelves(workspacePath: string): Promise<Shelf[]> {
  return createDraftlineClient().listShelves(workspacePath);
}

export async function previewShelf(request: ShelfRequest): Promise<VersionPreview> {
  return createDraftlineClient().previewShelf(request);
}

export async function preflightApplyShelf(request: ShelfRequest): Promise<ShelfApplyReport> {
  return createDraftlineClient().preflightApplyShelf(request);
}

export async function applyShelf(request: ShelfRequest): Promise<ApplyShelfCommandResult> {
  return createDraftlineClient().applyShelf(request);
}

export async function deleteShelf(request: ShelfRequest): Promise<DeleteShelfResult> {
  return createDraftlineClient().deleteShelf(request);
}

export async function auditContentPolicy(workspacePath: string): Promise<ContentPolicyAudit> {
  return createDraftlineClient().auditContentPolicy(workspacePath);
}

export async function clearStaleLock(workspacePath: string): Promise<CommandPostconditions> {
  return createDraftlineClient().clearStaleLock(workspacePath);
}

export async function repairRecovery(request: RecoveryRequest): Promise<RecoveryRepairResult> {
  return createDraftlineClient().repairRecovery(request);
}

export async function rollbackRecovery(request: RecoveryRequest): Promise<RecoveryRepairResult> {
  return createDraftlineClient().rollbackRecovery(request);
}

export async function selectedSave(request: SelectedSaveRequest): Promise<SelectedSaveResult> {
  return createDraftlineClient().selectedSave(request);
}

export async function selectedShelve(request: SelectedShelveRequest): Promise<SelectedShelveResult> {
  return createDraftlineClient().selectedShelve(request);
}

export async function selectedDiscard(
  request: SelectedDiscardRequest,
): Promise<SelectedDiscardResult> {
  return createDraftlineClient().selectedDiscard(request);
}

export async function publishCurrentVariation(
  request: PublishCurrentVariationRequest,
): Promise<PublishCurrentVariationResult> {
  return createDraftlineClient().publishCurrentVariation(request);
}

export async function fetchRemote(request: RemoteRequest): Promise<FetchRemoteResult> {
  return createDraftlineClient().fetchRemote(request);
}

export async function preflightApplyIncoming(request: RemoteRequest): Promise<ApplyIncomingReport> {
  return createDraftlineClient().preflightApplyIncoming(request);
}

export async function applyIncoming(
  request: RemoteRequest,
): Promise<ApplyIncomingCommandResult> {
  return createDraftlineClient().applyIncoming(request);
}

export async function preflightMergeIncoming(request: RemoteRequest): Promise<MergeIncomingReport> {
  return createDraftlineClient().preflightMergeIncoming(request);
}

export async function mergeIncoming(
  request: MergeIncomingRequest,
): Promise<MergeIncomingCommandResult> {
  return createDraftlineClient().mergeIncoming(request);
}

export async function mergeIncomingWithResolutions(
  request: MergeIncomingWithResolutionsRequest,
): Promise<MergeIncomingCommandResult> {
  return createDraftlineClient().mergeIncomingWithResolutions(request);
}
