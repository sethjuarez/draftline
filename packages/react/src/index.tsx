import {
  createDraftlineClient,
  type ChangedFile,
  type DraftlineClient,
  type JsonValue,
  type SupportRef,
  type SupportRefScope,
  type Variation,
  type TauriCommandError,
  type Version,
  type WorkspaceDiagnostics,
} from '@draftline/client';
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type CSSProperties,
  type FormEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from 'react';

export type CommandResult = {
  name: string;
  payload: unknown;
  ok: boolean;
};

export type InspectorTab = 'json' | 'recovery' | 'supportRefs';

export type DraftlineMutation<T> = {
  error: unknown;
  isPending: boolean;
  reset: () => void;
  run: () => Promise<T | null>;
};

export type DraftlineGraphNodeKind = 'dirty' | 'version' | 'remoteTip';
export type DraftlineGraphDirtyMode = 'node' | 'none';
export type DraftlineGraphOrientation = 'horizontal' | 'vertical';
export type DraftlineGraphLaneOrder =
  | 'activeFirst'
  | ((left: DraftlineGraphLane, right: DraftlineGraphLane) => number);

export interface DraftlineGraphLane {
  id: string;
  label: string;
  variationId?: string | null;
}

interface DraftlineGraphNodeBase {
  id: string;
  kind: DraftlineGraphNodeKind;
  lane: number;
  row: number;
  label: string;
  variationId?: string | null;
  variationIds: string[];
  parentVersionIds: string[];
  isActiveHead: boolean;
  isHead: boolean;
}

export interface DraftlineVersionGraphNode extends DraftlineGraphNodeBase {
  kind: 'version';
  version: Version;
  versionId: string;
}

export interface DraftlineDirtyGraphNode extends DraftlineGraphNodeBase {
  kind: 'dirty';
  baseVersionId: string | null;
  changedCount: number;
  changedFiles: ChangedFile[];
}

export interface DraftlineRemoteTipGraphNode extends DraftlineGraphNodeBase {
  kind: 'remoteTip';
  remote: string;
  versionId?: string;
}

export type DraftlineGraphNode =
  | DraftlineDirtyGraphNode
  | DraftlineRemoteTipGraphNode
  | DraftlineVersionGraphNode;

export interface DraftlineGraphEdge {
  from: string;
  to: string;
  kind: 'parent' | 'remoteTip';
}

export interface DraftlineHistoryGraphModel {
  dirtyNode?: DraftlineGraphNode;
  edges: DraftlineGraphEdge[];
  lanes: DraftlineGraphLane[];
  nodes: DraftlineGraphNode[];
  orientation: DraftlineGraphOrientation;
}

export interface RemoteTipInput {
  id: string;
  label?: string;
  remote: string;
  variationId?: string | null;
  versionId?: string | null;
}

export interface DraftlineHistoryGraphInput {
  activeVariationId?: string | null;
  dirtyFiles?: ChangedFile[];
  dirtyMode?: DraftlineGraphDirtyMode;
  includeRemoteTips?: boolean;
  laneOrder?: DraftlineGraphLaneOrder;
  orientation?: DraftlineGraphOrientation;
  remoteTips?: RemoteTipInput[];
  variations?: Variation[];
  versions?: Version[];
}

export interface DraftlineGraphNodeContext {
  graph: DraftlineHistoryGraphModel;
  selectable: boolean;
}

export interface DraftlineGraphSelectionEvent extends DraftlineGraphNodeContext {
  nativeEvent?: ReactKeyboardEvent<HTMLButtonElement> | ReactMouseEvent<HTMLButtonElement>;
  node: DraftlineGraphNode;
}

export interface DraftlineHistoryGraphProps {
  ariaLabel?: string;
  className?: string;
  getNodeKey?: (node: DraftlineGraphNode) => string;
  graph: DraftlineHistoryGraphModel;
  onSelectNode?: (event: DraftlineGraphSelectionEvent) => Promise<void> | void;
  orientation?: DraftlineGraphOrientation;
  renderBadge?: (node: DraftlineGraphNode, context: DraftlineGraphNodeContext) => ReactNode;
  renderNode?: (node: DraftlineGraphNode, context: DraftlineGraphNodeContext) => ReactNode;
  style?: CSSProperties;
}

export interface VariationOptionRenderState {
  isActive: boolean;
  label: string;
  switchVariation: () => void;
}

export interface DraftlineVariationListItem {
  id: string;
  isActive: boolean;
  label: string;
  variation: Variation;
}

export interface DraftlineVariationListModel {
  activeVariationId?: string | null;
  items: DraftlineVariationListItem[];
}

export interface DraftlineVariationListInput {
  activeVariationId?: string | null;
  getLabel?: (variation: Variation) => string;
  variations: Variation[];
}

export interface DraftlineVariationSelectProps {
  className?: string;
  activeVariationId?: string | null;
  getLabel?: (variation: Variation) => string;
  onSwitch?: (variation: Variation) => void;
  renderOption?: (variation: Variation, state: VariationOptionRenderState) => ReactNode;
  style?: CSSProperties;
  variations: Variation[];
}

export interface DraftlineProviderProps {
  children: ReactNode;
  client?: DraftlineClient;
  workspacePath: string;
  onError?: (error: unknown, command: string) => void;
}

interface DraftlineContextValue {
  client: DraftlineClient;
  commandResult: CommandResult | null;
  diagnostics: WorkspaceDiagnostics | null;
  error: unknown;
  isBusy: boolean;
  refresh: (options?: RefreshOptions) => Promise<WorkspaceDiagnostics | null>;
  runCommand: <T>(
    name: string,
    command: () => Promise<T>,
    options?: RefreshOptions,
  ) => Promise<T | null>;
  supportRefs: SupportRef[];
  workspacePath: string;
}

interface RefreshOptions {
  manageBusy?: boolean;
  recordResult?: boolean;
  rethrow?: boolean;
}

const DraftlineContext = createContext<DraftlineContextValue | null>(null);

function normalizeWorkspacePath(path: string) {
  return path.replace(/\\/g, '/').replace(/\/+$/g, '');
}

export function DraftlineProvider({
  children,
  client: clientProp,
  onError,
  workspacePath,
}: DraftlineProviderProps) {
  const client = useMemo(() => clientProp ?? createDraftlineClient(), [clientProp]);
  const [commandResult, setCommandResult] = useState<CommandResult | null>(null);
  const [diagnostics, setDiagnostics] = useState<WorkspaceDiagnostics | null>(null);
  const [supportRefs, setSupportRefs] = useState<SupportRef[]>([]);
  const [error, setError] = useState<unknown>(null);
  const [isBusy, setIsBusy] = useState(false);

  const runCommand = useCallback<DraftlineContextValue['runCommand']>(
    async (name, command, options = {}) => {
      const manageBusy = options.manageBusy ?? true;
      const recordResult = options.recordResult ?? true;
      if (manageBusy) {
        setIsBusy(true);
      }
      setError(null);
      try {
        const payload = await command();
        if (recordResult) {
          setCommandResult({ name, payload, ok: true });
        }
        return payload;
      } catch (caught) {
        setError(caught);
        onError?.(caught, name);
        if (recordResult) {
          setCommandResult({ name, payload: normalizeCommandError(caught), ok: false });
        }
        if (options.rethrow) {
          throw caught;
        }
        return null;
      } finally {
        if (manageBusy) {
          setIsBusy(false);
        }
      }
    },
    [onError],
  );

  const refresh = useCallback(
    async (options: RefreshOptions = {}) => {
      const recordResult = options.recordResult ?? true;
      if (!workspacePath.trim()) {
        const payload = {
          code: 'missing_workspace_path',
          message: 'Enter a workspace path first.',
        };
        if (recordResult) {
          setCommandResult({ name: 'inspect_workspace', ok: false, payload });
        }
        setDiagnostics(null);
        setSupportRefs([]);
        return null;
      }

      setIsBusy(true);
      try {
        const next = await runCommand(
          'inspect_workspace',
          () => client.inspectWorkspace(workspacePath),
          { manageBusy: false, recordResult },
        );
        if (next) {
          setDiagnostics(next);
          const refs = await runCommand(
            'list_support_refs',
            () => client.listSupportRefs(workspacePath, 'local'),
            { manageBusy: false, recordResult: false },
          );
          setSupportRefs(refs ?? []);
        }
        return next;
      } finally {
        setIsBusy(false);
      }
    },
    [client, runCommand, workspacePath],
  );

  useEffect(() => {
    setDiagnostics(null);
    setSupportRefs([]);
    setCommandResult(null);
    setError(null);
  }, [workspacePath]);

  useEffect(() => {
    if (!workspacePath.trim()) {
      return;
    }

    let disposed = false;
    let unlisten: (() => void) | null = null;
    const activeWorkspace = normalizeWorkspacePath(workspacePath.trim());

    client
      .subscribeWorkspaceEvents((event) => {
        if (normalizeWorkspacePath(event.workspace_id.root) !== activeWorkspace) {
          return;
        }
        void refresh({ recordResult: false });
      })
      .then((nextUnlisten) => {
        if (disposed) {
          nextUnlisten();
          return;
        }
        unlisten = nextUnlisten;
      })
      .catch((eventError: unknown) => {
        if (!disposed) {
          setError(eventError);
        }
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [client, refresh, workspacePath]);

  const value = useMemo<DraftlineContextValue>(
    () => ({
      client,
      commandResult,
      diagnostics,
      error,
      isBusy,
      refresh,
      runCommand,
      supportRefs,
      workspacePath,
    }),
    [
      client,
      commandResult,
      diagnostics,
      error,
      isBusy,
      refresh,
      runCommand,
      supportRefs,
      workspacePath,
    ],
  );

  return <DraftlineContext.Provider value={value}>{children}</DraftlineContext.Provider>;
}

export function useDraftlineWorkspace() {
  const context = useDraftlineContext();
  return {
    diagnostics: context.diagnostics,
    error: context.error,
    isBusy: context.isBusy,
    refresh: context.refresh,
    workspacePath: context.workspacePath,
  };
}

export function useDraftlineChanges() {
  const { diagnostics } = useDraftlineContext();
  return {
    files: diagnostics?.summary.dirty_files ?? [],
    isDirty: diagnostics?.summary.is_dirty ?? false,
  };
}

export function useDraftlineInspector() {
  const context = useDraftlineContext();
  return {
    commandResult: context.commandResult,
    diagnostics: context.diagnostics,
    supportRefs: context.supportRefs,
  };
}

export function useDraftlineHistoryGraph(options: Omit<DraftlineHistoryGraphInput, 'variations' | 'versions' | 'dirtyFiles' | 'activeVariationId'> = {}) {
  const { diagnostics } = useDraftlineWorkspace();
  return useMemo(
    () =>
      createDraftlineHistoryGraph({
        activeVariationId: diagnostics?.summary.active_variation.id,
        dirtyFiles: diagnostics?.summary.dirty_files,
        dirtyMode: options.dirtyMode,
        includeRemoteTips: options.includeRemoteTips,
        laneOrder: options.laneOrder,
        orientation: options.orientation,
        remoteTips: options.remoteTips,
        variations: diagnostics?.summary.variations,
        versions: diagnostics?.summary.versions,
      }),
    [
      diagnostics,
      options.dirtyMode,
      options.includeRemoteTips,
      options.laneOrder,
      options.orientation,
      options.remoteTips,
    ],
  );
}

export function useDraftlineSupportRefs(scope: SupportRefScope = 'local') {
  const context = useDraftlineContext();
  const load = useDraftlineMutation('list_support_refs', () =>
    context.client.listSupportRefs(context.workspacePath, scope),
  );

  return {
    load,
    supportRefs: scope === 'local' ? context.supportRefs : [],
  };
}

export function useDraftlineRemoteSync(remote: string) {
  const context = useDraftlineContext();
  const request = { workspace_path: context.workspacePath, remote };

  const publish = useDraftlineMutation('publish_current_variation', async () => {
    const result = await context.client.publishCurrentVariation(request);
    await context.refresh({ recordResult: false });
    return result;
  });
  const fetch = useDraftlineMutation('fetch_remote', async () => {
    const result = await context.client.fetchRemote(request);
    await context.refresh({ recordResult: false });
    return result;
  });
  const preflightApply = useDraftlineMutation('preflight_apply_incoming', () =>
    context.client.preflightApplyIncoming(request),
  );
  const applyIncoming = useDraftlineMutation('apply_incoming', async () => {
    const result = await context.client.applyIncoming(request);
    await context.refresh({ recordResult: false });
    return result;
  });
  const preflightMerge = useDraftlineMutation('preflight_merge_incoming', () =>
    context.client.preflightMergeIncoming(request),
  );
  const mergeIncoming = useDraftlineMutation('merge_incoming', async () => {
    const result = await context.client.mergeIncoming({
      ...request,
      label: 'Workbench merge incoming',
    });
    await context.refresh({ recordResult: false });
    return result;
  });

  return {
    applyIncoming,
    fetch,
    mergeIncoming,
    preflightApply,
    preflightMerge,
    publish,
    remote,
  };
}

export function useDraftlineSelectedOperations(selectedPaths: string[]) {
  const context = useDraftlineContext();
  const request = { workspace_path: context.workspacePath, paths: selectedPaths };
  const save = useDraftlineMutation('selected_save', async () => {
    const result = await context.client.selectedSave({ ...request, label: 'Workbench save' });
    await context.refresh({ recordResult: false });
    return result;
  });
  const shelve = useDraftlineMutation('selected_shelve', async () => {
    const result = await context.client.selectedShelve({ ...request, name: 'workbench-shelf' });
    await context.refresh({ recordResult: false });
    return result;
  });
  const discard = useDraftlineMutation('selected_discard', async () => {
    const result = await context.client.selectedDiscard(request);
    await context.refresh({ recordResult: false });
    return result;
  });

  return { discard, save, shelve };
}

export function WorkspaceSummaryPanel() {
  const { diagnostics } = useDraftlineWorkspace();
  const summary = diagnostics?.summary;
  const inspection = diagnostics?.inspection;
  const lock = diagnostics?.operation_lock;

  return (
    <article className="panel panel-large">
      <div className="panel-heading">
        <h2>Workspace</h2>
        <StatusPill tone={summary?.is_dirty ? 'warn' : 'ok'}>
          {summary?.is_dirty ? 'Dirty' : 'Ready'}
        </StatusPill>
      </div>
      <dl className="metric-grid">
        <Metric label="Current variation" value={summary?.active_variation.name ?? '-'} />
        <Metric label="Versions" value={String(summary?.versions.length ?? 0)} />
        <Metric label="Dirty files" value={String(summary?.dirty_files.length ?? 0)} />
        <Metric label="Sharing" value={inspection?.sharing_mode ?? '-'} />
        <Metric label="Recovery" value={summary?.recovery ? 'Present' : 'Clear'} />
        <Metric label="Lock" value={lock?.state ?? '-'} />
      </dl>
      <div className="safe-actions">
        {(inspection?.safe_next_actions ?? ['normal_work']).map((action) => (
          <span key={action}>{action}</span>
        ))}
      </div>
    </article>
  );
}

export function WorkspaceStatusRail() {
  const { diagnostics, isBusy } = useDraftlineWorkspace();
  const summary = diagnostics?.summary;
  const inspection = diagnostics?.inspection;
  const lock = diagnostics?.operation_lock;

  return (
    <article className="panel status-rail">
      <h2>Status rail</h2>
      <div className="rail-items">
        <RailItem label="Contract" tone="ok" value="Tauri invoke" />
        <RailItem label="Command" tone={isBusy ? 'warn' : 'ok'} value={isBusy ? 'Busy' : 'Idle'} />
        <RailItem
          label="Content"
          tone={summary?.is_dirty ? 'warn' : 'ok'}
          value={summary?.is_dirty ? `${summary.dirty_files.length} dirty` : 'Clean'}
        />
        <RailItem
          label="Recovery"
          tone={summary?.recovery ? 'danger' : 'ok'}
          value={summary?.recovery ? summary.recovery.operation : 'Clear'}
        />
        <RailItem
          label="Lock"
          tone={lock?.state === 'locked' ? 'danger' : 'ok'}
          value={lock?.state ?? 'unknown'}
        />
        <RailItem
          label="Remote"
          tone={(inspection?.remotes.length ?? 0) > 0 ? 'ok' : 'neutral'}
          value={`${inspection?.remotes.length ?? 0} configured`}
        />
      </div>
    </article>
  );
}

export function SelectedOperationsPanel({
  selectedPaths,
  setSelectedPaths,
}: {
  selectedPaths: string;
  setSelectedPaths: (paths: string) => void;
}) {
  const selectedPathList = useMemo(() => splitSelectedPaths(selectedPaths), [selectedPaths]);

  return (
    <SelectedOperationsForm
      selectedPathList={selectedPathList}
      selectedPaths={selectedPaths}
      setSelectedPaths={setSelectedPaths}
    />
  );
}

function SelectedOperationsForm({
  selectedPathList,
  selectedPaths,
  setSelectedPaths,
}: {
  selectedPathList: string[];
  selectedPaths: string;
  setSelectedPaths: (paths: string) => void;
}) {
  const { isBusy } = useDraftlineWorkspace();
  const operations = useDraftlineSelectedOperations(selectedPathList);

  return (
    <form className="stack" onSubmit={(event) => void submitMutation(event, operations.save)}>
      <label htmlFor="selected-paths">Selected paths</label>
      <input
        id="selected-paths"
        value={selectedPaths}
        onChange={(event) => setSelectedPaths(event.target.value)}
        placeholder="post.md, notes/idea.md"
      />
      <div className="button-row">
        <button disabled={isBusy} type="submit">
          Save
        </button>
        <button
          disabled={isBusy}
          onClick={() => void operations.shelve.run()}
          type="button"
        >
          Shelve
        </button>
        <button
          disabled={isBusy}
          onClick={() => void operations.discard.run()}
          type="button"
        >
          Discard
        </button>
      </div>
    </form>
  );
}

export function OperationsPanel({
  remote,
  selectedPaths,
  setRemote,
  setSelectedPaths,
}: {
  remote: string;
  selectedPaths: string;
  setRemote: (remote: string) => void;
  setSelectedPaths: (paths: string) => void;
}) {
  return (
    <article className="panel">
      <h2>Operations</h2>
      <SelectedOperationsPanel selectedPaths={selectedPaths} setSelectedPaths={setSelectedPaths} />
      <RemoteSyncBar remote={remote} setRemote={setRemote} />
    </article>
  );
}

export function RemoteSyncBar({
  remote,
  setRemote,
}: {
  remote: string;
  setRemote: (remote: string) => void;
}) {
  const { isBusy } = useDraftlineWorkspace();
  const sync = useDraftlineRemoteSync(remote);

  return (
    <form className="stack" onSubmit={(event) => void submitMutation(event, sync.publish)}>
      <label htmlFor="remote-name">Remote</label>
      <div className="inline-form">
        <input
          id="remote-name"
          value={remote}
          onChange={(event) => setRemote(event.target.value)}
        />
        <button disabled={isBusy} type="submit">
          Publish
        </button>
      </div>
      <div className="button-row">
        <button disabled={isBusy} onClick={() => void sync.fetch.run()} type="button">
          Fetch
        </button>
        <button disabled={isBusy} onClick={() => void sync.preflightApply.run()} type="button">
          Preflight apply
        </button>
        <button disabled={isBusy} onClick={() => void sync.applyIncoming.run()} type="button">
          Apply incoming
        </button>
        <button disabled={isBusy} onClick={() => void sync.preflightMerge.run()} type="button">
          Preflight merge
        </button>
        <button disabled={isBusy} onClick={() => void sync.mergeIncoming.run()} type="button">
          Merge incoming
        </button>
      </div>
    </form>
  );
}

export function ChangedFilesList({ files }: { files?: ChangedFile[] }) {
  const contextFiles = useDraftlineChanges().files;
  const resolvedFiles = files ?? contextFiles;
  const groups = groupChangedFiles(resolvedFiles);

  return (
    <article className="panel">
      <h2>Dirty files</h2>
      {resolvedFiles.length === 0 ? (
        <p className="empty">No tracked content changes.</p>
      ) : (
        <div className="file-groups">
          {groups.map(([kind, groupedFiles]) => (
            <section key={kind} className="file-group">
              <div className="group-heading">
                <h3>{kind}</h3>
                <StatusPill tone="neutral">{String(groupedFiles.length)}</StatusPill>
              </div>
              <ul className="item-list">
                {groupedFiles.map((file) => (
                  <li key={file.path}>
                    <span>{file.path}</span>
                    <span className="badge-row">
                      {file.is_large ? <StatusPill tone="warn">large</StatusPill> : null}
                      {file.is_binary ? <StatusPill tone="warn">binary</StatusPill> : null}
                    </span>
                  </li>
                ))}
              </ul>
            </section>
          ))}
        </div>
      )}
    </article>
  );
}

export function VariationList() {
  const { diagnostics } = useDraftlineWorkspace();
  const variations = diagnostics?.summary.variations ?? [];
  const activeVariationId = diagnostics?.summary.active_variation.id;

  return (
    <article className="panel">
      <h2>Variations</h2>
      {variations.length === 0 ? (
        <p className="empty">Inspect a workspace to list variations.</p>
      ) : (
        <DraftlineVariationSelect activeVariationId={activeVariationId} variations={variations} />
      )}
    </article>
  );
}

export function VersionLane() {
  const graph = useDraftlineHistoryGraph();

  return (
    <article className="panel panel-large">
      <div className="panel-heading">
        <h2>Version lane</h2>
        {graph.dirtyNode ? <StatusPill tone="warn">unsaved changes</StatusPill> : null}
      </div>
      {graph.nodes.length === 0 ? (
        <p className="empty">Inspect a workspace to render version history.</p>
      ) : (
        <DraftlineHistoryGraph graph={graph} />
      )}
    </article>
  );
}

export function createDraftlineHistoryGraph({
  activeVariationId,
  dirtyFiles = [],
  dirtyMode = 'node',
  includeRemoteTips = true,
  laneOrder = 'activeFirst',
  orientation = 'vertical',
  remoteTips = [],
  variations = [],
  versions = [],
}: DraftlineHistoryGraphInput): DraftlineHistoryGraphModel {
  const activeVariation = variations.find((variation) => variation.id === activeVariationId);
  const activeLane: DraftlineGraphLane = {
    id: activeVariation?.id ?? activeVariationId ?? 'active',
    label: activeVariation?.metadata.label ?? activeVariation?.name ?? 'Current variation',
    variationId: activeVariation?.id ?? activeVariationId ?? null,
  };
  const lanes: DraftlineGraphLane[] = [activeLane];
  const nodes: DraftlineGraphNode[] = [];
  const edges: DraftlineGraphEdge[] = [];
  const activeVariationIds = activeLane.variationId ? [activeLane.variationId] : [];
  let rowOffset = 0;
  let dirtyNode: DraftlineGraphNode | undefined;

  if (dirtyMode === 'node' && dirtyFiles.length > 0) {
    dirtyNode = {
      baseVersionId: versions[0]?.id ?? null,
      changedFiles: dirtyFiles,
      changedCount: dirtyFiles.length,
      id: 'dirty',
      isActiveHead: true,
      isHead: true,
      kind: 'dirty',
      label: 'Unsaved changes',
      lane: 0,
      parentVersionIds: versions[0] ? [versions[0].id] : [],
      row: 0,
      variationId: activeLane.variationId,
      variationIds: activeVariationIds,
    };
    nodes.push(dirtyNode);
    rowOffset = 1;
  }

  versions.forEach((version, index) => {
    const node: DraftlineGraphNode = {
      id: `version:${version.id}`,
      isActiveHead: index === 0 && !dirtyNode,
      isHead: index === 0,
      kind: 'version',
      label: version.label || version.id.slice(0, 7),
      lane: 0,
      parentVersionIds: index + 1 < versions.length ? [versions[index + 1].id] : [],
      row: index + rowOffset,
      variationId: activeLane.variationId,
      variationIds: activeVariationIds,
      version,
      versionId: version.id,
    };
    nodes.push(node);

    if (index > 0) {
      edges.push({
        from: `version:${versions[index].id}`,
        kind: 'parent',
        to: `version:${versions[index - 1].id}`,
      });
    }
  });

  if (dirtyNode && versions[0]) {
    edges.push({ from: `version:${versions[0].id}`, kind: 'parent', to: dirtyNode.id });
  }

  const versionNodeIds = new Set(versions.map((version) => `version:${version.id}`));
  const remoteTipRowOffset = rowOffset + versions.length;
  const tips = includeRemoteTips ? remoteTips : [];
  tips.forEach((tip, index) => {
    const lane = lanes.findIndex((existing) => existing.id === `remote:${tip.remote}:${tip.variationId ?? tip.id}`);
    const laneIndex =
      lane >= 0
        ? lane
        : lanes.push({
            id: `remote:${tip.remote}:${tip.variationId ?? tip.id}`,
            label: tip.label ?? `${tip.remote}/${tip.variationId ?? tip.id}`,
            variationId: tip.variationId,
          }) - 1;
    const node: DraftlineGraphNode = {
      id: `remote:${tip.id}`,
      isActiveHead: false,
      isHead: true,
      kind: 'remoteTip',
      label: tip.label ?? tip.id,
      lane: laneIndex,
      parentVersionIds: tip.versionId ? [tip.versionId] : [],
      remote: tip.remote,
      row: remoteTipRowOffset + index,
      variationId: tip.variationId,
      variationIds: tip.variationId ? [tip.variationId] : [],
      versionId: tip.versionId ?? undefined,
    };
    nodes.push(node);
    if (tip.versionId && versionNodeIds.has(`version:${tip.versionId}`)) {
      edges.push({ from: `version:${tip.versionId}`, kind: 'remoteTip', to: node.id });
    }
  });

  const orderedLanes = orderGraphLanes(lanes, laneOrder);
  const orderedNodes = nodes.map((node) => ({
    ...node,
    lane: orderedLanes.indexByOriginalLane.get(node.lane) ?? node.lane,
  }));
  const orderedDirtyNode = dirtyNode
    ? orderedNodes.find((node) => node.id === dirtyNode.id && node.kind === 'dirty')
    : undefined;

  return {
    dirtyNode: orderedDirtyNode,
    edges,
    lanes: orderedLanes.lanes,
    nodes: orderedNodes,
    orientation,
  };
}

export function createDraftlineVariationListModel({
  activeVariationId,
  getLabel = defaultVariationLabel,
  variations,
}: DraftlineVariationListInput): DraftlineVariationListModel {
  return {
    activeVariationId,
    items: variations.map((variation) => ({
      id: variation.id,
      isActive: activeVariationId ? variation.id === activeVariationId : variation.is_current,
      label: getLabel(variation),
      variation,
    })),
  };
}

export function DraftlineHistoryGraph({
  ariaLabel = 'Draftline history graph',
  className,
  getNodeKey = (node) => node.id,
  graph,
  onSelectNode,
  orientation = graph.orientation,
  renderBadge,
  renderNode,
  style,
}: DraftlineHistoryGraphProps) {
  return (
    <ol
      aria-label={ariaLabel}
      className={['version-lane', className].filter(Boolean).join(' ')}
      data-orientation={orientation}
      data-testid="draftline-history-graph"
      id="draftline-history-graph"
      style={style}
    >
      {[...graph.nodes].sort(compareGraphNodes).map((node) => (
        <DraftlineHistoryGraphRow
          getNodeKey={getNodeKey}
          graph={graph}
          key={getNodeKey(node)}
          node={node}
          onSelectNode={onSelectNode}
          renderBadge={renderBadge}
          renderNode={renderNode}
        />
      ))}
    </ol>
  );
}

function DraftlineHistoryGraphRow({
  getNodeKey,
  graph,
  node,
  onSelectNode,
  renderBadge,
  renderNode,
}: {
  getNodeKey: (node: DraftlineGraphNode) => string;
  graph: DraftlineHistoryGraphModel;
  node: DraftlineGraphNode;
  onSelectNode?: (event: DraftlineGraphSelectionEvent) => Promise<void> | void;
  renderBadge?: (node: DraftlineGraphNode, context: DraftlineGraphNodeContext) => ReactNode;
  renderNode?: (node: DraftlineGraphNode, context: DraftlineGraphNodeContext) => ReactNode;
}) {
  const selectable = Boolean(onSelectNode);
  const context: DraftlineGraphNodeContext = { graph, selectable };
  const notifyNodeSelected = (
    nativeEvent?: ReactKeyboardEvent<HTMLButtonElement> | ReactMouseEvent<HTMLButtonElement>,
  ) => {
    const result = onSelectNode?.({ graph, nativeEvent, node, selectable });
    if (result) {
      void Promise.resolve(result).catch((error) => {
        setTimeout(() => {
          throw error;
        }, 0);
      });
    }
  };
  const handleKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (event.key !== 'Enter' && event.key !== ' ') {
      return;
    }

    // Preserve keyboard event context for hosts while avoiding a second synthesized click.
    event.preventDefault();
    notifyNodeSelected(event);
  };

  return (
    <li
      className={node.kind === 'dirty' ? 'version-row dirty-row' : 'version-row'}
      key={getNodeKey(node)}
      style={{ ['--draftline-lane' as string]: node.lane }}
    >
      <span className="graph-line">
        <span
          className={
            node.kind === 'dirty'
              ? 'graph-node dirty-node'
              : node.isHead
                ? 'graph-node head-node'
                : 'graph-node'
          }
        />
      </span>
      {onSelectNode ? (
        <button
          aria-current={node.isActiveHead ? 'step' : undefined}
          className="graph-node-button"
          onClick={(event) => notifyNodeSelected(event)}
          onKeyDown={handleKeyDown}
          type="button"
        >
          {renderNode ? renderNode(node, context) : <DefaultGraphNode node={node} />}
        </button>
      ) : (
        <div className="graph-node-content">
          {renderNode ? renderNode(node, context) : <DefaultGraphNode node={node} />}
        </div>
      )}
      <span className="badge-row">
        {renderBadge?.(node, context)}
        {node.isActiveHead ? <StatusPill tone="ok">HEAD</StatusPill> : null}
        {node.kind === 'remoteTip' ? <StatusPill tone="neutral">{node.remote}</StatusPill> : null}
      </span>
    </li>
  );
}

export function DraftlineVariationSelect({
  activeVariationId,
  className,
  getLabel = defaultVariationLabel,
  onSwitch,
  renderOption,
  style,
  variations,
}: DraftlineVariationSelectProps) {
  const model = createDraftlineVariationListModel({ activeVariationId, getLabel, variations });

  return (
    <ul
      className={['item-list', className].filter(Boolean).join(' ')}
      data-testid="draftline-variation-select"
      id="draftline-variation-select"
      style={style}
    >
      {model.items.map(({ isActive, label, variation }) => {
        const switchVariation = () => onSwitch?.(variation);
        return (
          <li key={variation.id}>
            {renderOption ? (
              renderOption(variation, { isActive, label, switchVariation })
            ) : (
              onSwitch ? (
                <button className="variation-option" onClick={switchVariation} type="button">
                  <span>{label}</span>
                  {isActive ? <StatusPill tone="ok">current</StatusPill> : null}
                </button>
              ) : (
                <div className="variation-option">
                  <span>{label}</span>
                  {isActive ? <StatusPill tone="ok">current</StatusPill> : null}
                </div>
              )
            )}
          </li>
        );
      })}
    </ul>
  );
}

export function RawInspectorPanel() {
  const [selectedTab, setSelectedTab] = useState<InspectorTab>('json');
  const { commandResult, diagnostics, supportRefs } = useDraftlineInspector();

  return (
    <article className="panel panel-json">
      <div className="panel-heading">
        <h2>Inspector</h2>
        {commandResult ? (
          <StatusPill tone={commandResult.ok ? 'ok' : 'danger'}>{commandResult.name}</StatusPill>
        ) : null}
      </div>
      <div className="tab-row">
        <TabButton current={selectedTab} setCurrent={setSelectedTab} tab="json">
          Raw JSON
        </TabButton>
        <TabButton current={selectedTab} setCurrent={setSelectedTab} tab="recovery">
          Recovery & locks
        </TabButton>
        <TabButton current={selectedTab} setCurrent={setSelectedTab} tab="supportRefs">
          Support refs
        </TabButton>
      </div>
      {selectedTab === 'json' ? <JsonPanel result={commandResult} /> : null}
      {selectedTab === 'recovery' ? <RecoveryPanel diagnostics={diagnostics} /> : null}
      {selectedTab === 'supportRefs' ? <SupportRefsPanel supportRefs={supportRefs} /> : null}
    </article>
  );
}

export function ContentPolicyDiagnosticsPanel() {
  const { diagnostics } = useDraftlineWorkspace();
  const diagnosticsList = diagnostics?.verification.diagnostics ?? [];

  return (
    <article className="panel">
      <h2>Content policy</h2>
      {diagnosticsList.length === 0 ? (
        <p className="empty">No content policy diagnostics.</p>
      ) : (
        <ul className="item-list">
          {diagnosticsList.map((diagnostic) => (
            <li key={`${diagnostic.code}-${diagnostic.message}`}>
              <span>{diagnostic.message}</span>
              <StatusPill tone={diagnostic.severity === 'blocking' ? 'danger' : 'warn'}>
                {diagnostic.code}
              </StatusPill>
            </li>
          ))}
        </ul>
      )}
    </article>
  );
}

function useDraftlineContext() {
  const context = useContext(DraftlineContext);
  if (!context) {
    throw new Error('Draftline hooks must be used inside DraftlineProvider.');
  }
  return context;
}

function useDraftlineMutation<T>(name: string, command: () => Promise<T>): DraftlineMutation<T> {
  const context = useDraftlineContext();
  const [error, setError] = useState<unknown>(null);
  const [isPending, setIsPending] = useState(false);

  return {
    error,
    isPending,
    reset: () => setError(null),
    run: async () => {
      setIsPending(true);
      setError(null);
      try {
        return await context.runCommand(name, command, { rethrow: true });
      } catch (caught) {
        setError(caught);
        return null;
      } finally {
        setIsPending(false);
      }
    },
  };
}

function RailItem({
  label,
  tone,
  value,
}: {
  label: string;
  tone: 'danger' | 'neutral' | 'ok' | 'warn';
  value: string;
}) {
  return (
    <div className="rail-item">
      <span>{label}</span>
      <StatusPill tone={tone}>{value}</StatusPill>
    </div>
  );
}

function DefaultGraphNode({ node }: { node: DraftlineGraphNode }) {
  if (node.kind === 'dirty') {
    return (
      <span>
        <strong>Unsaved changes</strong>
        <small>{node.changedCount ?? 0} tracked file(s) need save, shelf, or discard.</small>
      </span>
    );
  }

  if (node.kind === 'remoteTip') {
    return (
      <span>
        <strong>{node.label}</strong>
        <small>{node.remote ?? 'remote'} tip</small>
      </span>
    );
  }

  return (
    <span>
      <strong>{node.label}</strong>
      <small>
        {node.version?.author.name ?? 'Unknown'} -{' '}
        {node.version ? new Date(node.version.time_seconds * 1000).toLocaleString() : 'unsaved'}
      </small>
    </span>
  );
}

function defaultVariationLabel(variation: Variation) {
  return variation.metadata.label ?? variation.name;
}

function compareGraphNodes(left: DraftlineGraphNode, right: DraftlineGraphNode) {
  if (left.row !== right.row) {
    return left.row - right.row;
  }

  return left.lane - right.lane;
}

function orderGraphLanes(lanes: DraftlineGraphLane[], laneOrder: DraftlineGraphLaneOrder) {
  const indexed = lanes.map((lane, originalIndex) => ({ lane, originalIndex }));
  if (typeof laneOrder === 'function') {
    indexed.sort((left, right) => laneOrder(left.lane, right.lane));
  } else if (laneOrder === 'activeFirst') {
    indexed.sort((left, right) => {
      if (left.originalIndex === 0) {
        return -1;
      }
      if (right.originalIndex === 0) {
        return 1;
      }
      return left.originalIndex - right.originalIndex;
    });
  }

  const indexByOriginalLane = new Map<number, number>();
  indexed.forEach(({ originalIndex }, nextIndex) => {
    indexByOriginalLane.set(originalIndex, nextIndex);
  });

  return {
    indexByOriginalLane,
    lanes: indexed.map(({ lane }) => lane),
  };
}

function RecoveryPanel({ diagnostics }: { diagnostics: WorkspaceDiagnostics | null }) {
  const recovery = diagnostics?.summary.recovery;
  const lock = diagnostics?.operation_lock;
  const diagnosticsList = diagnostics?.verification.diagnostics ?? [];

  return (
    <div className="inspector-card">
      <dl className="metric-grid compact">
        <Metric label="Recovery" value={recovery ? recovery.operation : 'Clear'} />
        <Metric
          label="State inconsistent"
          value={String(diagnostics?.summary.state_may_be_inconsistent ?? false)}
        />
        <Metric label="Lock" value={lock?.state ?? '-'} />
      </dl>
      {diagnosticsList.length === 0 ? (
        <p className="empty">No verification diagnostics.</p>
      ) : (
        <ul className="item-list">
          {diagnosticsList.map((diagnostic) => (
            <li key={`${diagnostic.code}-${diagnostic.message}`}>
              <span>{diagnostic.message}</span>
              <StatusPill tone={diagnostic.severity === 'blocking' ? 'danger' : 'warn'}>
                {diagnostic.code}
              </StatusPill>
            </li>
          ))}
        </ul>
      )}
      {lock?.metadata ? <pre>{JSON.stringify(lock.metadata, null, 2)}</pre> : null}
    </div>
  );
}

function SupportRefsPanel({ supportRefs }: { supportRefs: SupportRef[] }) {
  return (
    <div className="inspector-card">
      {supportRefs.length === 0 ? (
        <p className="empty">No local recovery support refs.</p>
      ) : (
        <ul className="item-list">
          {supportRefs.map((supportRef) => (
            <li key={supportRef.id}>
              <span>{supportRef.source_variation ?? supportRef.id}</span>
              <StatusPill tone="neutral">{supportRef.kind}</StatusPill>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function JsonPanel({ result }: { result: CommandResult | null }) {
  return (
    <div className="inspector-card">
      <pre>{result ? JSON.stringify(result.payload, null, 2) : 'Run a command to inspect JSON.'}</pre>
    </div>
  );
}

function TabButton({
  children,
  current,
  setCurrent,
  tab,
}: {
  children: string;
  current: InspectorTab;
  setCurrent: (tab: InspectorTab) => void;
  tab: InspectorTab;
}) {
  return (
    <button
      className={current === tab ? 'tab-button tab-button-active' : 'tab-button'}
      onClick={() => setCurrent(tab)}
      type="button"
    >
      {children}
    </button>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function StatusPill({
  children,
  tone,
}: {
  children: string;
  tone: 'danger' | 'neutral' | 'ok' | 'warn';
}) {
  return <span className={`pill pill-${tone}`}>{children}</span>;
}

function normalizeCommandError(error: unknown): TauriCommandError | unknown {
  if (error && typeof error === 'object' && 'code' in error && 'message' in error) {
    return error;
  }

  return {
    code: 'unknown_error',
    message: error instanceof Error ? error.message : String(error),
  };
}

function groupChangedFiles(files: ChangedFile[]): Array<[ChangedFile['kind'], ChangedFile[]]> {
  const groups = new Map<ChangedFile['kind'], ChangedFile[]>();
  for (const file of files) {
    groups.set(file.kind, [...(groups.get(file.kind) ?? []), file]);
  }

  return [...groups.entries()];
}

function splitSelectedPaths(paths: string) {
  return paths
    .split(',')
    .map((path) => path.trim())
    .filter(Boolean);
}

async function submitMutation<T>(event: FormEvent, mutation: DraftlineMutation<T>) {
  event.preventDefault();
  await mutation.run();
}

export type { JsonValue };
export type { DraftlineClient } from '@draftline/client';
