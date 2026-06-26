import { StrictMode } from 'react';
import { initAuditaur } from '@auditaur/api';
import { createDraftlineClient, setDraftlineInvoke } from '@draftline/client';
import { createRoot } from 'react-dom/client';

import { App } from './App';
import './styles.css';

async function main() {
  const auditaur = await initAuditaur({
    serviceName: 'draftline-workbench-frontend',
    instrumentConsole: true,
    instrumentErrors: true,
    instrumentTauriInvoke: true,
    instrumentTauriEvents: true,
    batchIntervalMs: 500,
    driveBridge: {
      windowLabel: 'main',
    },
  });
  setDraftlineInvoke(auditaur.invoke);
  const client = createDraftlineClient({ invoke: auditaur.invoke });

  createRoot(document.getElementById('root') as HTMLElement).render(
    <StrictMode>
      <App client={client} />
    </StrictMode>,
  );
}

main().catch((error) => {
  console.error('Failed to initialize Draftline Workbench', error);
});
