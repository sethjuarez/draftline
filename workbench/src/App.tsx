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
        <WorkbenchDashboard />
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

function WorkbenchDashboard() {
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
      <RawInspectorPanel />
    </section>
  );
}
