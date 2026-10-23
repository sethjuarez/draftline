# Draftline React

React hooks and primitive components for Draftline-enabled Tauri applications.

`@draftline/react` builds on `@draftline/client` and provides provider-backed
workspace state, mutation helpers, remote/recovery surfaces, and headless-first
graph and variation primitives. The primitives are designed for host apps to keep
their own shell, copy, styling, and destructive-operation policy.

## Install

```powershell
npm install @draftline/client @draftline/react
```

## Usage

```tsx
import { createDraftlineClient } from '@draftline/client';
import {
  DraftlineHistoryGraph,
  DraftlineWorkspaceGraph,
  DraftlineProvider,
  createDraftlineHistoryGraph,
  createDraftlineWorkspaceGraphModel,
} from '@draftline/react';
import '@xyflow/react/dist/style.css';

const client = createDraftlineClient();

export function App() {
  const graph = createDraftlineHistoryGraph({
    activeVariationId: 'main',
    versions: [],
    variations: [],
  });

  return (
    <DraftlineProvider client={client} workspacePath="C:\\path\\to\\workspace">
      <DraftlineHistoryGraph graph={graph} />
    </DraftlineProvider>
  );
}
```

For Git/DAG history, pass Draftline's `WorkspaceGraph` DTO directly into
`createDraftlineWorkspaceGraphModel`. Use `view: 'first-parent'` for a compact
main-line view and `view: 'expanded'` to show every saved side commit
and merge parent. Hosts can supply `renderNode` to use product-specific labels,
badges, and theme tokens while Draftline keeps the graph semantics.

## Design notes

- `DraftlineGraphNode` is an exported discriminated union for version, dirty, and
  remote-tip nodes.
- `DraftlineHistoryGraph` is non-interactive unless the host supplies
  `onSelectNode`.
- `DraftlineWorkspaceGraph` consumes `WorkspaceGraph`, supports first-parent and
  expanded views, and uses React Flow + Dagre for rendering/layout only.
- Selection callbacks receive full context: `{ node, graph, selectable,
  nativeEvent }`.
- Host apps own labels, editor reloads, dirty-navigation prompts, and remote UX.
