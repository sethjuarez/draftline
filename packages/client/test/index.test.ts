import { describe, expect, it, vi } from 'vitest';

import {
  createDraftlineHostFacade,
  createMergeConflictViewModel,
  createDraftlineClient,
  createWholeFileUseContentResolutions,
  type DraftlineInvoke,
  type MergeIncomingReport,
  type VariationRenameToken,
  type WorkspaceDiagnostics,
} from '../src/index';

describe('createDraftlineClient', () => {
  it('invokes Draftline Tauri commands with stable request DTO casing', async () => {
    const invoke = vi.fn<DraftlineInvoke>(async (command) => {
      if (command === 'inspect_workspace') {
        return fixtureDiagnostics as never;
      }
      return { ok: true } as never;
    });
    const client = createDraftlineClient({ invoke });

    await client.inspectWorkspace('C:\\repo');
    await client.fetchRemote({ workspace_path: 'C:\\repo', remote: 'origin' });
    await client.applyIncoming({ workspace_path: 'C:\\repo', remote: 'origin' });
    await client.mergeIncoming({
      workspace_path: 'C:\\repo',
      remote: 'origin',
      label: 'Merge incoming',
    });
    await client.mergeIncomingWithResolutions({
      workspace_path: 'C:\\repo',
      remote: 'origin',
      label: 'Merge resolved',
      token: {
        remote: 'origin',
        variation: 'main',
        local_oid: 'local',
        remote_oid: 'remote',
        merge_base_oid: 'base',
      },
      resolutions: [
        {
          path: 'post.md',
          field_path: null,
          choice: { kind: 'use_content', content: 'resolved' },
        },
      ],
    });
    await client.selectedSave({
      workspace_path: 'C:\\repo',
      paths: ['post.md'],
      label: 'Save post',
    });
    await client.selectedShelve({
      workspace_path: 'C:\\repo',
      paths: ['post.md'],
      name: 'post-shelf',
    });
    await client.selectedDiscard({
      workspace_path: 'C:\\repo',
      paths: ['post.md'],
    });
    await client.publishCurrentVariation({ workspace_path: 'C:\\repo', remote: 'origin' });
    await client.listSupportRefs('C:\\repo', 'local');
    await client.preflightRenameVariation({
      workspace_path: 'C:\\repo',
      source_variation_id: 'master',
      target_variation_id: 'main',
    });
    await client.renameVariation({
      workspace_path: 'C:\\repo',
      source_variation_id: 'master',
      target_variation_id: 'main',
    });

    expect(invoke).toHaveBeenNthCalledWith(1, 'inspect_workspace', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenNthCalledWith(2, 'fetch_remote', {
      request: { workspace_path: 'C:\\repo', remote: 'origin' },
    });
    expect(invoke).toHaveBeenNthCalledWith(3, 'apply_incoming', {
      request: { workspace_path: 'C:\\repo', remote: 'origin' },
    });
    expect(invoke).toHaveBeenNthCalledWith(4, 'merge_incoming', {
      request: { workspace_path: 'C:\\repo', remote: 'origin', label: 'Merge incoming' },
    });
    expect(invoke).toHaveBeenNthCalledWith(5, 'merge_incoming_with_resolutions', {
      request: {
        workspace_path: 'C:\\repo',
        remote: 'origin',
        label: 'Merge resolved',
        token: {
          remote: 'origin',
          variation: 'main',
          local_oid: 'local',
          remote_oid: 'remote',
          merge_base_oid: 'base',
        },
        resolutions: [
          {
            path: 'post.md',
            field_path: null,
            choice: { kind: 'use_content', content: 'resolved' },
          },
        ],
      },
    });
    expect(invoke).toHaveBeenNthCalledWith(6, 'selected_save', {
      request: { workspace_path: 'C:\\repo', paths: ['post.md'], label: 'Save post' },
    });
    expect(invoke).toHaveBeenNthCalledWith(7, 'selected_shelve', {
      request: { workspace_path: 'C:\\repo', paths: ['post.md'], name: 'post-shelf' },
    });
    expect(invoke).toHaveBeenNthCalledWith(8, 'selected_discard', {
      request: { workspace_path: 'C:\\repo', paths: ['post.md'] },
    });
    expect(invoke).toHaveBeenNthCalledWith(9, 'publish_current_variation', {
      request: { workspace_path: 'C:\\repo', remote: 'origin' },
    });
    expect(invoke).toHaveBeenNthCalledWith(10, 'list_support_refs', {
      request: { workspace_path: 'C:\\repo', scope: 'local' },
    });
    expect(invoke).toHaveBeenNthCalledWith(11, 'preflight_rename_variation', {
      request: {
        workspace_path: 'C:\\repo',
        source_variation_id: 'master',
        target_variation_id: 'main',
      },
    });
    expect(invoke).toHaveBeenNthCalledWith(12, 'rename_variation', {
      request: {
        workspace_path: 'C:\\repo',
        source_variation_id: 'master',
        target_variation_id: 'main',
      },
    });
  });

  it('emits command lifecycle callbacks for success and failure', async () => {
    const start = vi.fn();
    const success = vi.fn();
    const error = vi.fn();
    const failure = { code: 'boom', message: 'failed' };
    const client = createDraftlineClient({
      invoke: vi.fn<DraftlineInvoke>(async (command) => {
        if (command === 'verify_workspace') {
          throw failure;
        }
        return fixtureDiagnostics as never;
      }),
      onCommandError: error,
      onCommandStart: start,
      onCommandSuccess: success,
    });

    await client.inspectWorkspace('C:\\repo');
    await expect(client.verifyWorkspace('C:\\repo')).rejects.toBe(failure);

    expect(start).toHaveBeenCalledWith('inspect_workspace', {
      request: { workspace_path: 'C:\\repo' },
    });

    expect(success).toHaveBeenCalledWith('inspect_workspace', fixtureDiagnostics);
    expect(error).toHaveBeenCalledWith('verify_workspace', failure);
  });

  it('invokes expanded Draftline command coverage with stable request DTO casing', async () => {
    const invoke = vi.fn<DraftlineInvoke>(async () => ({ ok: true }) as never);
    const client = createDraftlineClient({ invoke });
    const versionId = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
    const versionRequest = { workspace_path: 'C:\\repo', version_id: versionId };
    const shelfRequest = { workspace_path: 'C:\\repo', shelf_id: 'draft-shelf' };
    const recoveryRequest = { workspace_path: 'C:\\repo', operation_id: 'op-1' };

    await client.getChanges('C:\\repo');
    await client.getHistory('C:\\repo');
    await client.getFullHistory('C:\\repo');
    await client.getWorkspaceGraph({
      workspace_path: 'C:\\repo',
      options: { include_remotes: true, remote: 'origin' },
    });
    await client.getWorkspaceGraphRefs({
      workspace_path: 'C:\\repo',
      options: { include_support_refs: true },
    });
    await client.getWorkspaceGraphSummary({
      workspace_path: 'C:\\repo',
      options: { include_remotes: true },
    });
    await client.getWorkspaceGraphOverview({
      workspace_path: 'C:\\repo',
      options: { include_remotes: true, max_nodes: 50, recent_nodes: 10 },
    });
    await client.getWorkspaceGraphAroundVersion({
      workspace_path: 'C:\\repo',
      version_id: versionId,
      radius: 3,
      options: { include_support_refs: true },
    });
    await client.getWorkspaceGraphForVariation({
      workspace_path: 'C:\\repo',
      variation_id: 'feature',
      options: { include_remotes: true },
    });
    await client.getWorkspaceGraphAgentSummary({
      workspace_path: 'C:\\repo',
      options: { include_remotes: true, include_support_refs: true },
    });
    await client.getWorkspaceGraphNeighborhood({
      workspace_path: 'C:\\repo',
      version_id: versionId,
      radius: 2,
      options: { include_remotes: true },
    });
    await client.searchWorkspaceGraph({
      workspace_path: 'C:\\repo',
      query: 'feature',
      options: { limit: 5 },
    });
    await client.getWorkspaceGraphPath({
      workspace_path: 'C:\\repo',
      from_version_id: versionId,
      to_version_id: versionId,
      options: { include_support_refs: true },
    });
    await client.getWorkspaceGraphCommonAncestor({
      workspace_path: 'C:\\repo',
      from_version_id: versionId,
      to_version_id: versionId,
    });
    await client.getWorkspaceGraphNode(versionRequest);
    await client.getWorkspaceGraphCompareSummary({
      workspace_path: 'C:\\repo',
      from_version_id: versionId,
      to_version_id: versionId,
    });
    await client.diffVersions({
      workspace_path: 'C:\\repo',
      from_version_id: versionId,
      to_version_id: versionId,
    });

    await client.diffVersionToWorkspace(versionRequest);
    await client.previewVersion(versionRequest);
    await client.previewVersionFile({ ...versionRequest, path: 'post.md' });
    await client.restoreVersionAsNewSave({ ...versionRequest, label: 'Restore' });
    await client.restoreVersionAsNewSaveToVariation({
      ...versionRequest,
      label: 'Restore targeted',
      target: { kind: 'existing', variation: 'alternate' },
    });
    await client.createVariationFromVersion({
      ...versionRequest,
      name: 'graph-branch',
      metadata: { label: 'Graph branch', slug: 'graph-branch' },
    });
    await client.listShelves('C:\\repo');
    await client.previewShelf(shelfRequest);
    await client.preflightApplyShelf(shelfRequest);
    await client.applyShelf(shelfRequest);
    await client.deleteShelf(shelfRequest);
    await client.auditContentPolicy('C:\\repo');
    await client.clearStaleLock('C:\\repo');
    await client.repairRecovery(recoveryRequest);
    await client.rollbackRecovery(recoveryRequest);

    expect(invoke).toHaveBeenCalledWith('get_changes', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('get_history', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('get_full_history', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph', {
      request: {
        workspace_path: 'C:\\repo',
        options: { include_remotes: true, remote: 'origin' },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_refs', {
      request: {
        workspace_path: 'C:\\repo',
        options: { include_support_refs: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_summary', {
      request: {
        workspace_path: 'C:\\repo',
        options: { include_remotes: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_overview', {
      request: {
        workspace_path: 'C:\\repo',
        options: { include_remotes: true, max_nodes: 50, recent_nodes: 10 },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_around_version', {
      request: {
        workspace_path: 'C:\\repo',
        version_id: versionId,
        radius: 3,
        options: { include_support_refs: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_for_variation', {
      request: {
        workspace_path: 'C:\\repo',
        variation_id: 'feature',
        options: { include_remotes: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_agent_summary', {
      request: {
        workspace_path: 'C:\\repo',
        options: { include_remotes: true, include_support_refs: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_neighborhood', {
      request: {
        workspace_path: 'C:\\repo',
        version_id: versionId,
        radius: 2,
        options: { include_remotes: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('search_workspace_graph', {
      request: {
        workspace_path: 'C:\\repo',
        query: 'feature',
        options: { limit: 5 },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_path', {
      request: {
        workspace_path: 'C:\\repo',
        from_version_id: versionId,
        to_version_id: versionId,
        options: { include_support_refs: true },
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_common_ancestor', {
      request: {
        workspace_path: 'C:\\repo',
        from_version_id: versionId,
        to_version_id: versionId,
      },
    });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_node', { request: versionRequest });
    expect(invoke).toHaveBeenCalledWith('get_workspace_graph_compare_summary', {
      request: {
        workspace_path: 'C:\\repo',
        from_version_id: versionId,
        to_version_id: versionId,
      },
    });
    expect(invoke).toHaveBeenCalledWith('diff_versions', {
      request: {
        workspace_path: 'C:\\repo',
        from_version_id: versionId,
        to_version_id: versionId,
      },
    });
    expect(invoke).toHaveBeenCalledWith('diff_version_to_workspace', { request: versionRequest });
    expect(invoke).toHaveBeenCalledWith('preview_version', { request: versionRequest });
    expect(invoke).toHaveBeenCalledWith('preview_version_file', {
      request: { ...versionRequest, path: 'post.md' },
    });
    expect(invoke).toHaveBeenCalledWith('restore_version_as_new_save', {
      request: { ...versionRequest, label: 'Restore' },
    });
    expect(invoke).toHaveBeenCalledWith('restore_version_as_new_save_to_variation', {
      request: {
        ...versionRequest,
        label: 'Restore targeted',
        target: { kind: 'existing', variation: 'alternate' },
      },
    });
    expect(invoke).toHaveBeenCalledWith('create_variation_from_version', {
      request: {
        ...versionRequest,
        name: 'graph-branch',
        metadata: { label: 'Graph branch', slug: 'graph-branch' },
      },
    });
    expect(invoke).toHaveBeenCalledWith('list_shelves', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('preview_shelf', { request: shelfRequest });
    expect(invoke).toHaveBeenCalledWith('preflight_apply_shelf', { request: shelfRequest });
    expect(invoke).toHaveBeenCalledWith('apply_shelf', { request: shelfRequest });
    expect(invoke).toHaveBeenCalledWith('delete_shelf', { request: shelfRequest });
    expect(invoke).toHaveBeenCalledWith('audit_content_policy', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('clear_stale_lock', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('repair_recovery', { request: recoveryRequest });
    expect(invoke).toHaveBeenCalledWith('rollback_recovery', { request: recoveryRequest });
  });

  it('invokes reusable host setup, file preview, facade, and remote variation commands', async () => {
    const invoke = vi.fn<DraftlineInvoke>(async (command) => {
      if (command === 'open_workspace' || command === 'clone_workspace') {
        return { diagnostics: fixtureDiagnostics } as never;
      }
      if (command === 'adopt_workspace') {
        return {
          diagnostics: fixtureDiagnostics,
          preflight: {
            blockers: [],
            can_adopt: true,
            candidate_policy_diagnostics: [],
            inspection: fixtureDiagnostics.inspection,
            safe_next_actions: ['normal_work'],
            warnings: [],
          },
        } as never;
      }
      return { ok: true } as never;
    });
    const client = createDraftlineClient({ invoke });
    const facade = createDraftlineHostFacade({ client, workspacePath: 'C:\\repo' });

    await client.openWorkspace('C:\\repo');
    await client.cloneWorkspace({
      remote_url: 'file:///remote.git',
      workspace_path: 'C:\\clone',
    });
    await client.adoptWorkspace('C:\\repo');
    await client.listRemotes('C:\\repo');
    await client.listRemoteVariations({ workspace_path: 'C:\\repo', remote: 'origin' });
    await client.remoteVariationDiagnostics({ workspace_path: 'C:\\repo', remote: 'origin' });
    await client.adoptRemoteVariation({
      workspace_path: 'C:\\repo',
      remote: 'origin',
      variation_id: 'teammate-option',
    });
    await client.diffWorkspaceFile({ workspace_path: 'C:\\repo', path: 'post.md' });
    await client.previewWorkspaceFile({ workspace_path: 'C:\\repo', path: 'post.md' });
    await facade.restoreAsNewSaveToVariation('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'Facade restore', {
      kind: 'new',
      name: 'snapshot-preview',
      metadata: { label: 'Snapshot Preview' },
    });
    const renameToken: VariationRenameToken = {
      operation_id: 'op-1',
      source_variation: 'master',
      target_variation: 'main',
      expected_oid: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
      support_ref: 'refs/draftline/deleted-variations/master/op-1',
    };
    await facade.preflightRenameVariation('master', 'main');
    await facade.renameVariation('master', 'main', renameToken);
    await facade.save('Facade save');

    expect(invoke).toHaveBeenCalledWith('open_workspace', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('clone_workspace', {
      request: { remote_url: 'file:///remote.git', workspace_path: 'C:\\clone' },
    });
    expect(invoke).toHaveBeenCalledWith('adopt_workspace', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('list_remotes', {
      request: { workspace_path: 'C:\\repo' },
    });
    expect(invoke).toHaveBeenCalledWith('list_remote_variations', {
      request: { workspace_path: 'C:\\repo', remote: 'origin' },
    });
    expect(invoke).toHaveBeenCalledWith('remote_variation_diagnostics', {
      request: { workspace_path: 'C:\\repo', remote: 'origin' },
    });
    expect(invoke).toHaveBeenCalledWith('adopt_remote_variation', {
      request: {
        workspace_path: 'C:\\repo',
        remote: 'origin',
        variation_id: 'teammate-option',
      },
    });
    expect(invoke).toHaveBeenCalledWith('diff_workspace_file', {
      request: { workspace_path: 'C:\\repo', path: 'post.md' },
    });
    expect(invoke).toHaveBeenCalledWith('preview_workspace_file', {
      request: { workspace_path: 'C:\\repo', path: 'post.md' },
    });
    expect(invoke).toHaveBeenCalledWith('restore_version_as_new_save_to_variation', {
      request: {
        workspace_path: 'C:\\repo',
        version_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
        label: 'Facade restore',
        target: {
          kind: 'new',
          name: 'snapshot-preview',
          metadata: { label: 'Snapshot Preview' },
        },
      },
    });
    expect(invoke).toHaveBeenCalledWith('preflight_rename_variation', {
      request: {
        workspace_path: 'C:\\repo',
        source_variation_id: 'master',
        target_variation_id: 'main',
      },
    });
    expect(invoke).toHaveBeenCalledWith('rename_variation', {
      request: {
        workspace_path: 'C:\\repo',
        source_variation_id: 'master',
        target_variation_id: 'main',
        token: renameToken,
      },
    });
    expect(invoke).toHaveBeenCalledWith('save', {
      request: { workspace_path: 'C:\\repo', label: 'Facade save' },
    });
  });

  it('groups merge conflicts and creates safest whole-file use_content resolutions', () => {
    const report: MergeIncomingReport = {
      can_merge_cleanly: false,
      changed_workspace: false,
      conflicts: [
        {
          base: 'base',
          field_path: null,
          label: 'post.md',
          ours: 'ours',
          path: 'post.md',
          resolution: 'Choose',
          theirs: 'theirs',
        },
        {
          base: 'base title',
          field_path: 'frontmatter.title',
          label: 'Title',
          ours: 'our title',
          path: 'post.md',
          resolution: 'Edit',
          theirs: 'their title',
        },
      ],
      dirty_files: [],
      file_hazards: [],
      sync_status: {
        ahead: 1,
        behind: 1,
        incoming: [],
        remote: 'origin',
        state: 'NeedsMerge',
        variation: 'main',
      },
      token: {
        local_oid: 'local',
        merge_base_oid: 'base',
        remote: 'origin',
        remote_oid: 'remote',
        variation: 'main',
      },
    };

    expect(createMergeConflictViewModel(report).files).toEqual([
      expect.objectContaining({
        field_conflicts: [
          expect.objectContaining({
            field_path: 'frontmatter.title',
          }),
        ],
        path: 'post.md',
        whole_file_conflicts: [expect.objectContaining({ ours: 'ours' })],
      }),
    ]);
    expect(createWholeFileUseContentResolutions(report, 'theirs')).toEqual([
      {
        choice: { kind: 'use_content', content: 'theirs' },
        field_path: null,
        path: 'post.md',
      },
    ]);
  });

  it('subscribes to the stable Draftline workspace event channel', async () => {
    const unlisten = vi.fn();
    const listen = vi.fn(async (_event, handler) => {
      handler({
        payload: {
          active_variation: 'main',
          changed_paths: ['post.md'],
          diagnostics: [],
          dirty: { files: [], is_dirty: false },
          kind: 'history_changed',
          recovery: null,
          sequence: 7,
          sync: null,
          workspace_id: { root: 'C:/repo/' },
        },
      });
      return unlisten;
    });
    const handler = vi.fn();
    const client = createDraftlineClient({
      invoke: vi.fn<DraftlineInvoke>(async () => fixtureDiagnostics as never),
      listen,
    });

    const unsubscribe = await client.subscribeWorkspaceEvents(handler);
    unsubscribe();

    expect(listen).toHaveBeenCalledWith('draftline://workspace_event', expect.any(Function));
    expect(handler).toHaveBeenCalledWith(
      expect.objectContaining({
        changed_paths: ['post.md'],
        kind: 'history_changed',
        sequence: 7,
      }),
    );
    expect(unlisten).toHaveBeenCalled();
  });
});

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
    support_refs: { local_count: 0, remote_count: 0 },
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
    versions: [],
  },
  verification: {
    current_variation_present: true,
    diagnostics: [],
    operation_lock_clear: true,
    recovery_clear: true,
  },
};
