# Draftline Workbench

Workbench is the repo-local Tauri host for exercising Draftline through real
frontend invokes, Rust command wrappers, filesystem state, and Git remotes.

Run the frontend build from the repository root:

```powershell
npm install
npm run build
```

Run the Tauri app during local validation:

```powershell
npm run workbench:dev
```

The Rust command wrappers live in `workbench/src-tauri/src/main.rs` and delegate
to `draftline::tauri_contract`, so the workbench stays aligned with the crate's
host-facing contract.
