import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type {
  DraftlineClient,
  DraftlineEvent,
  DraftlineEventHandler,
  FetchRemoteResult,
  WorkspaceDiagnostics,
} from '@draftline/client';
import {
  ChangedFilesList,
  ContentPolicyDiagnosticsPanel,
  createDraftlineHistoryGraph,
  createDraftlineVariationListModel,
  DraftlineHistoryGraph,
  DraftlineProvider,
  DraftlineVariationSelect,
  RawInspectorPanel,
  RemoteSyncBar,
  SelectedOperationsPanel,
  useDraftlineRemoteSync,
  useDraftlineWorkspace,
} from '../src/index';
import { useState } from 'react';

afterEach(() => cleanup());

describe('@draftline/react', () => {
  it('loads workspace diagnostics and support refs through the provider lifecycle', async () => {
    const client = fakeClient();

    render(
      <DraftlineProvider client={client} workspacePath={workspacePath}>
        <WorkspaceProbe />
        <RawInspectorPanel />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));

    await screen.findByText('main');
    fireEvent.click(screen.getByRole('button', { name: 'Support refs' }));
    await screen.findByText(/active support ref/);
    expect(client.inspectWorkspace).toHaveBeenCalledWith(workspacePath);
    expect(client.listSupportRefs).toHaveBeenCalledWith(workspacePath, 'local');
  });

  it('refreshes diagnostics when matching Draftline workspace events arrive', async () => {
    let eventHandler: DraftlineEventHandler | null = null;
    const unlisten = vi.fn();
    const client = fakeClient({
      subscribeWorkspaceEvents: vi.fn(async (handler) => {
        eventHandler = handler;
        return unlisten;
      }),
    });

    render(
      <DraftlineProvider client={client} workspacePath={workspacePath}>
        <RawInspectorPanel />
      </DraftlineProvider>,
    );

    await waitFor(() => expect(client.subscribeWorkspaceEvents).toHaveBeenCalledTimes(1));
    eventHandler?.(workspaceEvent('dirty_changed', 'C:/repo/'));

    await waitFor(() => expect(client.inspectWorkspace).toHaveBeenCalledWith(workspacePath));
    expect(client.listSupportRefs).toHaveBeenCalledWith(workspacePath, 'local');
  });

  it('wires RemoteSyncBar actions to the client and refreshes after mutations', async () => {
    const client = fakeClient();

    render(
      <DraftlineProvider client={client} workspacePath={workspacePath}>
        <RemoteSyncBar remote="origin" setRemote={() => undefined} />
        <RawInspectorPanel />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Fetch' }));
    await waitFor(() => expect(client.fetchRemote).toHaveBeenCalledTimes(1));
    expect(client.inspectWorkspace).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByRole('button', { name: 'Preflight apply' }));
    await waitFor(() => expect(client.preflightApplyIncoming).toHaveBeenCalledTimes(1));
    await screen.findByText(/can_proceed/);

    fireEvent.click(screen.getByRole('button', { name: 'Apply incoming' }));
    await waitFor(() => expect(client.applyIncoming).toHaveBeenCalledTimes(1));
    expect(client.inspectWorkspace).toHaveBeenCalledTimes(2);
  });

  it('exposes mutation errors without hiding the command result', async () => {
    const failure = { code: 'sync_failed', message: 'remote unavailable' };
    const client = fakeClient({
      fetchRemote: vi.fn(async () => {
        throw failure;
      }),
    });

    render(
      <DraftlineProvider client={client} workspacePath={workspacePath}>
        <RemoteErrorProbe />
        <RawInspectorPanel />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Fetch with hook' }));

    await screen.findByText('remote unavailable');
    await screen.findByText(/sync_failed/);
  });

  it('keeps ChangedFilesList hook order stable when callers toggle controlled files', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined);

    render(
      <DraftlineProvider client={fakeClient()} workspacePath={workspacePath}>
        <ChangedFilesListToggle />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Use controlled files' }));
    await screen.findByText('controlled.md');
    fireEvent.click(screen.getByRole('button', { name: 'Use provider files' }));
    await screen.findByText('No tracked content changes.');

    expect(consoleError).not.toHaveBeenCalled();
    consoleError.mockRestore();
  });

  it('reports a missing workspace path without invoking the backend', async () => {
    const client = fakeClient();

    render(
      <DraftlineProvider client={client} workspacePath=" ">
        <WorkspaceProbe />
        <RawInspectorPanel />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));

    await screen.findByText(/missing_workspace_path/);
    expect(client.inspectWorkspace).not.toHaveBeenCalled();
  });

  it('keeps selected-operation preflight failures visible without refreshing success state', async () => {
    const failure = {
      code: 'preflight_failed',
      details: {
        can_proceed: false,
        dirty_files: [],
        operation: 'save_files',
      },
      message: 'selected save preflight failed',
    };
    const client = fakeClient({
      selectedSave: vi.fn(async () => {
        throw failure;
      }),
    });

    render(
      <DraftlineProvider client={client} workspacePath={workspacePath}>
        <SelectedOperationsPanel
          selectedPaths="missing.md"
          setSelectedPaths={() => undefined}
        />
        <RawInspectorPanel />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await screen.findByText(/preflight_failed/);
    await screen.findByText(/save_files/);
    expect(client.selectedSave).toHaveBeenCalledWith({
      label: 'Workbench save',
      paths: ['missing.md'],
      workspace_path: workspacePath,
    });
    expect(client.inspectWorkspace).not.toHaveBeenCalled();
  });

  it('renders content-policy diagnostics from workspace verification', async () => {
    const client = fakeClient({
      inspectWorkspace: vi.fn(async () => ({
        ...fixtureDiagnostics,
        verification: {
          ...fixtureDiagnostics.verification,
          diagnostics: [
            {
              code: 'ignored_policy_content',
              message: 'tracked content is hidden by .gitignore',
              severity: 'warning',
            },
          ],
        },
      })),
    });

    render(
      <DraftlineProvider client={client} workspacePath={workspacePath}>
        <WorkspaceProbe />
        <ContentPolicyDiagnosticsPanel />
      </DraftlineProvider>,
    );

    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));

    await screen.findByText('tracked content is hidden by .gitignore');
    await screen.findByText('ignored_policy_content');
  });

  it('creates a headless graph model with a dirty pseudo-node and parent edges', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      dirtyFiles: [{ is_binary: false, is_large: false, kind: 'Modified', path: 'post.md' }],
      variations: fixtureDiagnostics.summary.variations,
      versions: [
        version,
        {
          ...version,
          id: 'parent456',
          label: 'Parent',
          time_seconds: 0,
        },
      ],
    });

    expect(graph.dirtyNode?.changedCount).toBe(1);
    expect(graph.orientation).toBe('vertical');
    expect(graph.nodes.includes(graph.dirtyNode!)).toBe(true);
    expect(graph.nodes.filter((node) => node.isActiveHead)).toEqual([
      expect.objectContaining({
        baseVersionId: 'abc123',
        changedFiles: [{ is_binary: false, is_large: false, kind: 'Modified', path: 'post.md' }],
        id: 'dirty',
        kind: 'dirty',
        parentVersionIds: ['abc123'],
        variationId: 'main',
      }),
    ]);
    expect(graph.nodes.map((node) => node.id)).toEqual([
      'dirty',
      'version:abc123',
      'version:parent456',
    ]);
    expect(graph.edges).toEqual([
      { from: 'version:parent456', kind: 'parent', to: 'version:abc123' },
      { from: 'version:abc123', kind: 'parent', to: 'dirty' },
    ]);
  });

  it('models remote tips as separate lanes without dangling edges', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      remoteTips: [
        {
          id: 'known-remote',
          label: 'origin/main',
          remote: 'origin',
          variationId: 'main',
          versionId: 'abc123',
        },
        {
          id: 'unknown-remote',
          remote: 'origin',
          variationId: 'teammate',
          versionId: 'missing',
        },
      ],
      variations: fixtureDiagnostics.summary.variations,
      versions: [version],
    });

    expect(graph.lanes.map((lane) => lane.label)).toEqual([
      'main',
      'origin/main',
      'origin/teammate',
    ]);
    expect(graph.nodes.find((node) => node.id === 'remote:known-remote')).toEqual(
      expect.objectContaining({ lane: 1, row: 1 }),
    );
    expect(graph.nodes.find((node) => node.id === 'remote:unknown-remote')).toEqual(
      expect.objectContaining({ lane: 2, row: 2 }),
    );
    expect(graph.edges).toEqual([
      { from: 'version:abc123', kind: 'remoteTip', to: 'remote:known-remote' },
    ]);
    expect(graph.nodes.find((node) => node.id === 'remote:known-remote')).toEqual(
      expect.objectContaining({
        kind: 'remoteTip',
        parentVersionIds: ['abc123'],
        variationId: 'main',
      }),
    );
  });

  it('supports graph options for dirty mode, remote inclusion, and orientation', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      dirtyFiles: [{ is_binary: false, is_large: false, kind: 'Modified', path: 'post.md' }],
      dirtyMode: 'none',
      includeRemoteTips: false,
      orientation: 'horizontal',
      remoteTips: [{ id: 'remote-main', remote: 'origin', variationId: 'main', versionId: 'abc123' }],
      variations: fixtureDiagnostics.summary.variations,
      versions: [version],
    });

    expect(graph.orientation).toBe('horizontal');
    expect(graph.dirtyNode).toBeUndefined();
    expect(graph.nodes.map((node) => node.kind)).toEqual(['version']);
    expect(graph.edges).toEqual([]);
  });

  it('supports custom graph lane ordering without hidden presets', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      laneOrder: (left, right) => right.label.localeCompare(left.label),
      remoteTips: [{ id: 'remote-main', label: 'origin/main', remote: 'origin', variationId: 'main', versionId: 'abc123' }],
      variations: fixtureDiagnostics.summary.variations,
      versions: [version],
    });

    expect(graph.lanes.map((lane) => lane.label)).toEqual(['origin/main', 'main']);
    expect(graph.nodes.find((node) => node.kind === 'remoteTip')).toEqual(
      expect.objectContaining({ lane: 0 }),
    );
    expect(graph.nodes.find((node) => node.kind === 'version')).toEqual(
      expect.objectContaining({ lane: 1 }),
    );
  });

  it('does not render inert default graph buttons when selection is omitted', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      variations: fixtureDiagnostics.summary.variations,
      versions: [version],
    });

    render(<DraftlineHistoryGraph graph={graph} />);

    expect(screen.queryByRole('button', { name: /Version/ })).toBeNull();
    expect(screen.getByRole('list', { name: 'Draftline history graph' })).toBeTruthy();
    expect(screen.getByText('Version')).toBeTruthy();
  });

  it('lets hosts customize graph node rendering without owning layout math', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      variations: fixtureDiagnostics.summary.variations,
      versions: [version],
    });
    const selected = vi.fn();

    render(
      <DraftlineHistoryGraph
        ariaLabel="Custom history"
        className="custom-history"
        getNodeKey={(node) => `custom-${node.id}`}
        graph={graph}
        onSelectNode={selected}
        orientation="horizontal"
        renderBadge={(node, context) => (
          <span>
            badge:{node.kind}:{String(context.selectable)}:{context.graph.nodes.length}
          </span>
        )}
        renderNode={(node, context) => (
          <span>
            custom:{node.label}:{context.graph.orientation}
          </span>
        )}
        style={{ color: 'red' }}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: /custom:Version:vertical/ }));

    expect(screen.getByRole('list', { name: 'Custom history' }).getAttribute('data-orientation')).toBe(
      'horizontal',
    );
    expect(screen.getByText('badge:version:true:1')).toBeTruthy();
    expect(selected).toHaveBeenCalledWith(
      expect.objectContaining({
        graph,
        nativeEvent: expect.any(Object),
        node: expect.objectContaining({ id: 'version:abc123', kind: 'version', versionId: 'abc123' }),
        selectable: true,
      }),
    );
  });

  it('supports keyboard selection when graph nodes are interactive', () => {
    const graph = createDraftlineHistoryGraph({
      activeVariationId: 'main',
      variations: fixtureDiagnostics.summary.variations,
      versions: [version],
    });
    const selected = vi.fn();

    render(<DraftlineHistoryGraph graph={graph} onSelectNode={selected} />);

    const button = screen.getByRole('button', { name: /Version/ });
    button.focus();
    fireEvent.keyDown(button, { key: 'Enter' });

    expect(document.activeElement).toBe(button);
    expect(button.getAttribute('aria-current')).toBe('step');
    expect(selected).toHaveBeenCalledWith(
      expect.objectContaining({
        nativeEvent: expect.objectContaining({ key: 'Enter' }),
        node: expect.objectContaining({ kind: 'version', versionId: 'abc123' }),
        selectable: true,
      }),
    );
    expect(selected).toHaveBeenCalledTimes(1);

    selected.mockClear();
    fireEvent.keyDown(button, { key: ' ' });

    expect(selected).toHaveBeenCalledWith(
      expect.objectContaining({
        nativeEvent: expect.objectContaining({ key: ' ' }),
        node: expect.objectContaining({ kind: 'version', versionId: 'abc123' }),
        selectable: true,
      }),
    );
    expect(selected).toHaveBeenCalledTimes(1);
  });

  it('creates a headless variation list model', () => {
    const model = createDraftlineVariationListModel({
      activeVariationId: 'main',
      getLabel: (variation) => `label:${variation.name}`,
      variations: fixtureDiagnostics.summary.variations,
    });

    expect(model).toEqual({
      activeVariationId: 'main',
      items: [
        {
          id: 'main',
          isActive: true,
          label: 'label:main',
          variation: fixtureDiagnostics.summary.variations[0],
        },
      ],
    });
  });

  it('lets hosts customize variation options and switch behavior', () => {
    const onSwitch = vi.fn();

    render(
      <DraftlineVariationSelect
        activeVariationId="main"
        onSwitch={onSwitch}
        renderOption={(variation, state) => (
          <button onClick={state.switchVariation} type="button">
            {state.label}:{String(state.isActive)}
          </button>
        )}
        variations={fixtureDiagnostics.summary.variations}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: 'main:true' }));

    expect(onSwitch).toHaveBeenCalledWith(fixtureDiagnostics.summary.variations[0]);
  });

  const workspacePath = 'C:\\repo';
});

function WorkspaceProbe() {
  const workspace = useDraftlineWorkspace();
  return (
    <div>
      <button onClick={() => void workspace.refresh()} type="button">
        Refresh
      </button>
      <span>{workspace.diagnostics?.summary.active_variation.name ?? 'empty'}</span>
    </div>
  );
}

function RemoteErrorProbe() {
  const sync = useDraftlineRemoteSync('origin');
  return (
    <div>
      <button onClick={() => void sync.fetch.run()} type="button">
        Fetch with hook
      </button>
      <span>{errorMessage(sync.fetch.error)}</span>
    </div>
  );
}

function ChangedFilesListToggle() {
  const [controlled, setControlled] = useState(false);
  return (
    <div>
      <button onClick={() => setControlled((current) => !current)} type="button">
        {controlled ? 'Use provider files' : 'Use controlled files'}
      </button>
      <ChangedFilesList
        files={
          controlled
            ? [
                {
                  is_binary: false,
                  is_large: false,
                  kind: 'Modified',
                  path: 'controlled.md',
                },
              ]
            : undefined
        }
      />
    </div>
  );
}

function fakeClient(overrides: Partial<DraftlineClient> = {}): DraftlineClient {
  return {
    applyIncoming: vi.fn(async () => ({
      apply: { applied_count: 1 },
      postconditions: {
        errors: [],
        remaining_changes: { diff: '', files: [] },
        verification: fixtureDiagnostics.verification,
      },
      preflight: {
        can_proceed: true,
        dirty_files: [],
        file_hazards: [],
        is_fast_forward: true,
        sync_status: syncStatus,
      },
    })),
    applyShelf: vi.fn(async () => ({
      postconditions: { errors: [] },
      preflight: {
        can_proceed: true,
        dirty_files: [],
        file_hazards: [],
        shelf: { id: 'shelf', version },
      },
      shelf: { id: 'shelf', version },
    })),
    auditContentPolicy: vi.fn(async () => ({
      current_diagnostics: [],
      historical_out_of_policy_paths: [],
    })),
    clearStaleLock: vi.fn(async () => ({ errors: [] })),
    deleteShelf: vi.fn(async () => ({ postconditions: { errors: [] } })),
    diffVersionToWorkspace: vi.fn(async () => ({
      files: [],
      from_version: version.id,
      patch: null,
      to_version: null,
    })),
    diffVersions: vi.fn(async () => ({
      files: [],
      from_version: version.id,
      patch: null,
      to_version: version.id,
    })),
    fetchRemote: vi.fn(async () => fetchResult),
    getChanges: vi.fn(async () => ({ files: [] })),
    getFullHistory: vi.fn(async () => []),
    getHistory: vi.fn(async () => []),
    inspectWorkspace: vi.fn(async () => fixtureDiagnostics),
    listShelves: vi.fn(async () => []),
    listSupportRefs: vi.fn(async () => [
      {
        id: 'support-1',
        kind: 'deleted_variation',
        ref_name: 'refs/draftline/support/deleted/support-1',
        scope: 'local',
        source_variation: 'active support ref',
        target_oid: 'abc123',
      },
    ]),
    listVariations: vi.fn(async () => []),
    mergeIncoming: vi.fn(async () => ({
      merge: { merged_files: [], version: version },
      postconditions: { errors: [] },
      preflight: {
        can_merge_cleanly: true,
        changed_workspace: false,
        conflicts: [],
        dirty_files: [],
        file_hazards: [],
        sync_status: syncStatus,
        token: null,
      },
    })),
    mergeIncomingWithResolutions: vi.fn(async () => ({
      merge: { merged_files: [], version: version },
      postconditions: { errors: [] },
      preflight: {
        can_merge_cleanly: false,
        changed_workspace: false,
        conflicts: [],
        dirty_files: [],
        file_hazards: [],
        sync_status: syncStatus,
        token: null,
      },
    })),
    preflightApplyIncoming: vi.fn(async () => ({
      can_proceed: true,
      dirty_files: [],
      file_hazards: [],
      is_fast_forward: true,
      sync_status: syncStatus,
    })),
    preflightApplyShelf: vi.fn(async () => ({
      can_proceed: true,
      dirty_files: [],
      file_hazards: [],
      shelf: { id: 'shelf', version },
    })),
    preflightMergeIncoming: vi.fn(async () => ({
      can_merge_cleanly: true,
      changed_workspace: false,
      conflicts: [],
      dirty_files: [],
      file_hazards: [],
      sync_status: syncStatus,
      token: null,
    })),
    previewShelf: vi.fn(async () => ({ files: [], id: version.id })),
    previewVersion: vi.fn(async () => ({ files: [], id: version.id })),
    previewVersionFile: vi.fn(async () => null),
    publishCurrentVariation: vi.fn(async () => ({
      postconditions: { errors: [] },
      preflight: {
        can_publish: true,
        local_oid: 'abc',
        remote: 'origin',
        sync_status: syncStatus,
        token: null,
        variation: 'main',
      },
      publish: { published_versions: 1, remote: 'origin', variation: 'main' },
    })),
    repairRecovery: vi.fn(async () => ({
      changed_workspace: false,
      completed: true,
      operation: 'repair',
      operation_id: 'op-1',
      safe_next_actions: ['normal_work'],
    })),
    restoreVersionAsNewSave: vi.fn(async () => ({
      postconditions: { errors: [] },
      version,
    })),
    rollbackRecovery: vi.fn(async () => ({
      changed_workspace: false,
      completed: true,
      operation: 'rollback',
      operation_id: 'op-1',
      safe_next_actions: ['normal_work'],
    })),
    selectedDiscard: vi.fn(async () => ({
      discarded: { files: [] },
      postconditions: { errors: [] },
      preflight: preflightReport,
    })),
    selectedSave: vi.fn(async () => ({
      postconditions: { errors: [] },
      preflight: preflightReport,
      version,
    })),
    selectedShelve: vi.fn(async () => ({
      postconditions: { errors: [] },
      preflight: preflightReport,
      shelf: { id: 'shelf', version },
    })),
    subscribeWorkspaceEvents: vi.fn(async () => () => undefined),
    verifyWorkspace: vi.fn(async () => fixtureDiagnostics.verification),
    ...overrides,
  };
}

function workspaceEvent(kind: DraftlineEvent['kind'], root: string): DraftlineEvent {
  return {
    active_variation: 'main',
    changed_paths: ['post.md'],
    diagnostics: [],
    dirty: { files: [], is_dirty: false },
    kind,
    recovery: null,
    sequence: 1,
    sync: null,
    workspace_id: { root },
  };
}

function errorMessage(error: unknown) {
  if (error && typeof error === 'object' && 'message' in error) {
    return String(error.message);
  }
  return '';
}

const version = {
  author: { name: 'Author', email: null },
  id: 'abc123',
  label: 'Version',
  saved_by: { name: 'Author', email: null },
  time_seconds: 1,
};

const syncStatus = {
  ahead: 0,
  behind: 1,
  incoming: [
    {
      author: { name: 'Teammate', email: null },
      id: 'def456',
      label: 'Incoming',
      time_seconds: 2,
    },
  ],
  remote: 'origin',
  state: 'IncomingAvailable' as const,
  variation: 'main',
};

const fetchResult: FetchRemoteResult = {
  postconditions: { errors: [] },
  sync_status: syncStatus,
};

const preflightReport = {
  binary_files: [],
  can_proceed: true,
  dirty_files: [],
  file_hazards: [],
  large_files: [],
  operation: 'selected',
  unresolved_conflicts: [],
  untracked_assets: [],
  will_write_files: true,
};

const fixtureDiagnostics: WorkspaceDiagnostics = {
  inspection: {
    current_variation: 'main',
    diagnostics: [],
    dirty: { files: [], is_dirty: false },
    operation_lock: { state: 'unlocked' },
    recovery: null,
    remotes: [{ name: 'origin', url: 'file:///remote.git' }],
    safe_next_actions: ['normal_work'],
    sharing_mode: 'shared_capable',
    support_refs: { local_count: 1, remote_count: 0 },
    workspace_id: { root: 'C:/repo/' },
  },
  operation_lock: {
    can_clear: false,
    diagnostics: [],
    is_stale: false,
    metadata: null,
    state: 'unlocked',
  },
  summary: {
    active_variation: {
      id: 'main',
      is_current: true,
      metadata: { label: null, slug: null },
      name: 'main',
    },
    dirty_files: [],
    is_dirty: false,
    recovery: null,
    state_may_be_inconsistent: false,
    variations: [
      {
        id: 'main',
        is_current: true,
        metadata: { label: null, slug: null },
        name: 'main',
      },
    ],
    versions: [version],
  },
  verification: {
    current_variation_present: true,
    diagnostics: [],
    operation_lock_clear: true,
    recovery_clear: true,
  },
};
