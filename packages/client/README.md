# Draftline client

`client/` contains the private TypeScript command client used by Workbench.
It wraps Tauri `invoke` calls and mirrors the serializable shapes from
`draftline::tauri_contract`.

Build from the repository root:

```powershell
npm run client:build
```
