# Draftline client

`client/` contains the TypeScript command client used by Workbench and host apps.
It wraps Tauri `invoke` calls and mirrors the serializable shapes from
`draftline::tauri_contract`.

The client also exposes the stable Draftline workspace event channel used by
configured Tauri hosts:

```ts
const client = createDraftlineClient();
const unlisten = await client.subscribeWorkspaceEvents((event) => {
  if (event.kind === 'history_changed' || event.kind === 'dirty_changed') {
    // Refresh product UI from inspectWorkspace.
  }
});
```

Remote merge flows use the same typed DTOs as `draftline::tauri_contract`.
Call `preflightMergeIncoming` first; if it returns conflicts and a token with
`can_merge_cleanly: false`, use `createMergeConflictViewModel` to render
file/field groups and `createWholeFileUseContentResolutions` when the product UI
offers whole-file "use ours/theirs/base" actions. Those helpers emit explicit
`use_content` resolutions so the submitted payload matches the content the user
reviewed:

```ts
await client.mergeIncomingWithResolutions({
  workspace_path: workspacePath,
  remote: 'origin',
  label: 'Resolve teammate changes',
  token: preflight.token,
  resolutions: createWholeFileUseContentResolutions(preflight, 'theirs'),
});
```

For host code, `createDraftlineHostFacade` binds a client to one workspace path
and exposes product-level operations such as `save`, `selectedSave`,
`diffWorkspaceFile`, `previewWorkspaceFile`, `fetchRemote`, `mergeIncoming`,
`mergeIncomingWithResolutions`, recovery repair/rollback, and remote variation
adoption without repeating request DTO plumbing in every component.

## Workspace graph integration

`@draftline/client` exports the graph DTOs and helper return types directly,
including `WorkspaceGraph`, `WorkspaceGraphNode`, `WorkspaceGraphRef`,
`WorkspaceGraphActionHint`, `WorkspaceGraphBoundary`,
`WorkspaceGraphSearchResult`, `WorkspaceGraphPath`,
`WorkspaceGraphNodeDetail`, and `WorkspaceGraphCompareSummary`.

Prefer bounded graph calls for product UI:

```ts
const facade = createDraftlineHostFacade({ workspacePath });

const overview = await facade.workspaceGraphOverview({
  max_nodes: 80,
  recent_nodes: 40,
});
const refs = await facade.workspaceGraphRefs();
const focus = await facade.workspaceGraphNeighborhood(versionId, 2);
```

Use `workspaceGraphRefs` for cheap ref/badge refreshes, `overview` or
`workspaceGraphForVariation` for default panels, and `workspaceGraphAroundVersion`,
`workspaceGraphNeighborhood`, `searchWorkspaceGraph`, `workspaceGraphPath`, and
`workspaceGraphCommonAncestor` for focused expansion. Reserve the full
`workspaceGraph` call for explicit expanded history views.

Treat graph node IDs as opaque. Use the path/common-ancestor helpers for graph
relationships instead of parsing IDs. Bounded results can be partial; use
`was_pruned`, `has_more`, `next_cursor`, and each node's `boundary` fields to
render hidden edges. Layout hints are advisory for stable rendering within a
graph snapshot. Node and ref `action_hints` include stable action IDs, command
names, workspace-switch/version-creating flags, and a `disabled_reason` when an
action is unsafe.

Build from the repository root:

```powershell
npm run client:build
```
