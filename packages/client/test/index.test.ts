import { describe, expect, it, vi } from 'vitest';

import {
  createDraftlineClient,
  type DraftlineInvoke,
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
    expect(invoke).toHaveBeenNthCalledWith(5, 'selected_save', {
      request: { workspace_path: 'C:\\repo', paths: ['post.md'], label: 'Save post' },
    });
    expect(invoke).toHaveBeenNthCalledWith(6, 'selected_shelve', {
      request: { workspace_path: 'C:\\repo', paths: ['post.md'], name: 'post-shelf' },
    });
    expect(invoke).toHaveBeenNthCalledWith(7, 'selected_discard', {
      request: { workspace_path: 'C:\\repo', paths: ['post.md'] },
    });
    expect(invoke).toHaveBeenNthCalledWith(8, 'publish_current_variation', {
      request: { workspace_path: 'C:\\repo', remote: 'origin' },
    });
    expect(invoke).toHaveBeenNthCalledWith(9, 'list_support_refs', {
      request: { workspace_path: 'C:\\repo', scope: 'local' },
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
