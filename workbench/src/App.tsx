import {
  ChangedFilesList,
  DraftlineProvider,
  OperationsPanel,
  RawInspectorPanel,
  VariationList,
  VersionLane,
  WorkspaceStatusRail,
  WorkspaceSummaryPanel,
  useDraftlineWorkspace,
  type DraftlineClient,
} from '@draftline/react';
import { type WorkspaceGraph } from '@draftline/client';
import { type FormEvent, useState } from 'react';

export function App({ client }: { client?: DraftlineClient }) {
  const [workspacePath, setWorkspacePath] = useState('');

  return (
    <DraftlineProvider client={client} workspacePath={workspacePath}>
      <main className="app-shell">
        <section className="hero">
          <div>
            <p className="eyebrow">Draftline Workbench</p>
            <h1>Contract-first Tauri validation</h1>
            <p>
              Exercise Draftline through reusable package hooks and components backed by the same
              typed invoke boundary a host Tauri app uses.
            </p>
          </div>
          <WorkspaceForm setWorkspacePath={setWorkspacePath} workspacePath={workspacePath} />
        </section>
        <WorkbenchDashboard client={client} workspacePath={workspacePath} />
      </main>
    </DraftlineProvider>
  );
}

function WorkspaceForm({
  setWorkspacePath,
  workspacePath,
}: {
  setWorkspacePath: (workspacePath: string) => void;
  workspacePath: string;
}) {
  const { isBusy, refresh } = useDraftlineWorkspace();

  async function inspect(event: FormEvent) {
    event.preventDefault();
    await refresh();
  }

  return (
    <form className="workspace-form" onSubmit={(event) => void inspect(event)}>
      <label htmlFor="workspace-path">Workspace path</label>
      <div className="inline-form">
        <input
          id="workspace-path"
          value={workspacePath}
          onChange={(event) => setWorkspacePath(event.target.value)}
          placeholder="C:\\path\\to\\draftline-workspace"
        />
        <button disabled={isBusy} type="submit">
          Inspect
        </button>
      </div>
    </form>
  );
}

function WorkbenchDashboard({
  client,
  workspacePath,
}: {
  client?: DraftlineClient;
  workspacePath: string;
}) {
  const [remote, setRemote] = useState('origin');
  const [selectedPaths, setSelectedPaths] = useState('');

  return (
    <section className="dashboard-grid">
      <WorkspaceSummaryPanel />
      <OperationsPanel
        remote={remote}
        selectedPaths={selectedPaths}
        setRemote={setRemote}
        setSelectedPaths={setSelectedPaths}
      />
      <WorkspaceStatusRail />
      <ChangedFilesList />
      <VariationList />
      <VersionLane />
      <WorkspaceGraphPanel client={client} workspacePath={workspacePath} />
      <RawInspectorPanel />
    </section>
  );
}

function WorkspaceGraphPanel({
  client,
  workspacePath,
}: {
  client?: DraftlineClient;
  workspacePath: string;
}) {
  const [graph, setGraph] = useState<WorkspaceGraph | null>(null);
  const [includeRemotes, setIncludeRemotes] = useState(false);
  const [includeSupportRefs, setIncludeSupportRefs] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isLoading, setIsLoading] = useState(false);

  async function loadGraph() {
    if (!client || !workspacePath) {
      return;
    }
    setIsLoading(true);
    setError(null);
    try {
      const nextGraph = await client.getWorkspaceGraph({
        workspace_path: workspacePath,
        options: {
          include_remotes: includeRemotes,
          include_support_refs: includeSupportRefs,
        },
      });
      setGraph(nextGraph);
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setIsLoading(false);
    }
  }

  return (
    <section className="panel panel-large" data-testid="workspace-graph-panel">
      <div className="panel-heading">
        <h2>Workspace graph</h2>
        <button disabled={!workspacePath || isLoading} onClick={() => void loadGraph()} type="button">
          {isLoading ? 'Loading graph...' : 'Load graph'}
        </button>
      </div>
      <div className="graph-options">
        <label>
          <input
            checked={includeRemotes}
            onChange={(event) => setIncludeRemotes(event.target.checked)}
            type="checkbox"
          />
          Include remotes
        </label>
        <label>
          <input
            checked={includeSupportRefs}
            onChange={(event) => setIncludeSupportRefs(event.target.checked)}
            type="checkbox"
          />
          Include support refs
        </label>
      </div>
      {error ? <p className="error-text">{error}</p> : null}
      {graph ? (
        <div className="stack" data-testid="workspace-graph-summary">
          <dl className="metric-grid">
            <div>
              <dt>Nodes</dt>
              <dd>{graph.nodes.length}</dd>
            </div>
            <div>
              <dt>Refs</dt>
              <dd>{graph.refs.length}</dd>
            </div>
            <div>
              <dt>Current</dt>
              <dd>{graph.current_variation ?? 'none'}</dd>
            </div>
          </dl>
          <ul className="item-list graph-list">
            {graph.nodes.slice(0, 8).map((node) => (
              <li key={node.id}>
                <span>{node.version.label || node.version.id.slice(0, 7)}</span>
                <span className="badge-row">
                  <span className="pill pill-neutral">{node.kind}</span>
                  <span className="pill pill-ok">{node.parent_ids.length} parent(s)</span>
                </span>
              </li>
            ))}
          </ul>
        </div>
      ) : (
        <p className="muted">Load the graph to inspect the full-history DAG shape.</p>
      )}
    </section>
  );
}
