import { invoke as tauriInvoke } from '@tauri-apps/api/core';

export type DraftlineInvoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

export interface DraftlineClientOptions {
  invoke?: DraftlineInvoke;
  onCommandStart?: (command: string, args?: Record<string, unknown>) => void;
  onCommandSuccess?: (command: string, result: unknown) => void;
  onCommandError?: (command: string, error: unknown) => void;
}

export interface DraftlineClient {
  inspectWorkspace(workspacePath: string): Promise<WorkspaceDiagnostics>;
  verifyWorkspace(workspacePath: string): Promise<WorkspaceVerification>;
  listVariations(workspacePath: string): Promise<VariationSummary[]>;
  listSupportRefs(workspacePath: string, scope: SupportRefScope): Promise<SupportRef[]>;
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
}

let invokeDraftline: DraftlineInvoke = (command, args) => tauriInvoke(command, args);

export function setDraftlineInvoke(invoke: DraftlineInvoke) {
  invokeDraftline = invoke;
}

export function createDraftlineClient(options: DraftlineClientOptions = {}): DraftlineClient {
  const invoke = options.invoke ?? invokeDraftline;

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
    listSupportRefs: (workspacePath, scope) =>
      run('list_support_refs', {
        request: { workspace_path: workspacePath, scope } satisfies ListSupportRefsRequest,
      }),
    selectedSave: (request) => run('selected_save', { request }),
    selectedShelve: (request) => run('selected_shelve', { request }),
    selectedDiscard: (request) => run('selected_discard', { request }),
    publishCurrentVariation: (request) => run('publish_current_variation', { request }),
    fetchRemote: (request) => run('fetch_remote', { request }),
    preflightApplyIncoming: (request) => run('preflight_apply_incoming', { request }),
    applyIncoming: (request) => run('apply_incoming', { request }),
    preflightMergeIncoming: (request) => run('preflight_merge_incoming', { request }),
    mergeIncoming: (request) => run('merge_incoming', { request }),
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
export type SafeNextAction =
  | 'normal_work'
  | 'save_first'
  | 'discard_changes'
  | 'repair_recovery'
  | 'configure_remote';
export type SharingMode = 'local_only' | 'shared_capable';
export type SupportRefScope = 'local' | 'remote_tracking';
export type SupportRefKind = 'deleted_variation' | 'rewrite';
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

export interface VariationSummary {
  variation: Variation;
  head_version?: Version | null;
  reachable_version_count: number;
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
  label: string;
}

export interface MergeIncomingReport {
  sync_status: SyncStatus;
  dirty_files: ChangedFile[];
  file_hazards: FileHazard[];
  conflicts: MergeConflict[];
  token?: JsonValue | null;
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

export interface ListSupportRefsRequest extends WorkspaceRequest {
  scope: SupportRefScope;
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

export interface FetchRemoteResult {
  sync_status: SyncStatus;
  postconditions: CommandPostconditions;
}

export interface MergeIncomingRequest extends RemoteRequest {
  label: string;
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

export async function listSupportRefs(
  workspacePath: string,
  scope: SupportRefScope,
): Promise<SupportRef[]> {
  return createDraftlineClient().listSupportRefs(workspacePath, scope);
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
