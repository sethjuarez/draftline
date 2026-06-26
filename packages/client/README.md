# Draftline client

`client/` contains the private TypeScript command client used by Workbench.
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

Build from the repository root:

```powershell
npm run client:build
```
