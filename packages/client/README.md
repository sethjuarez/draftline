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
`can_merge_cleanly: false`, collect explicit resolutions and call
`mergeIncomingWithResolutions`:

```ts
await client.mergeIncomingWithResolutions({
  workspace_path: workspacePath,
  remote: 'origin',
  label: 'Resolve teammate changes',
  token: preflight.token,
  resolutions: [
    {
      path: 'content/post.md',
      choice: { kind: 'use_content', content: resolvedMarkdown },
    },
  ],
});
```

Build from the repository root:

```powershell
npm run client:build
```
