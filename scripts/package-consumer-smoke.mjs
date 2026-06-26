import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const root = process.cwd();
const tempRoot = mkdtempSync(join(tmpdir(), 'draftline-package-smoke-'));
const consumerRoot = join(tempRoot, 'consumer');

function npm(args, cwd = root, options = {}) {
  return execNpm(args, { cwd, stdio: 'inherit', ...options });
}

function execNpm(args, options) {
  if (process.platform !== 'win32') {
    return execFileSync('npm', args, options);
  }

  return execFileSync(process.env.ComSpec ?? 'cmd.exe', ['/d', '/c', 'npm', ...args], options);
}

function pack(workspace) {
  const dryRunOutput = npm(['pack', '--workspace', workspace, '--dry-run', '--json'], root, {
    encoding: 'utf8',
    stdio: 'pipe',
  });
  const [dryRun] = JSON.parse(dryRunOutput);
  const files = dryRun?.files?.map((file) => file.path) ?? [];
  if (!files.includes('dist/index.js') || !files.includes('dist/index.d.ts')) {
    throw new Error(`${workspace} pack output is missing built dist exports`);
  }
  if (!files.includes('README.md')) {
    throw new Error(`${workspace} pack output is missing README.md`);
  }

  const output = npm(['pack', '--workspace', workspace, '--pack-destination', tempRoot, '--json'], root, {
    encoding: 'utf8',
    stdio: 'pipe',
  });
  const [packed] = JSON.parse(output);
  if (!packed?.filename) {
    throw new Error(`npm pack did not return a filename for ${workspace}`);
  }
  return join(tempRoot, packed.filename);
}

npm(['run', 'build', '--workspace', '@draftline/client']);
npm(['run', 'build', '--workspace', '@draftline/react']);

const clientTarball = pack('@draftline/client');
const reactTarball = pack('@draftline/react');

mkdirSync(consumerRoot);
writeFileSync(join(consumerRoot, 'package.json'), JSON.stringify({ private: true, type: 'module' }, null, 2));
npm(['install', clientTarball, reactTarball, 'react@^19.0.0'], consumerRoot);
npm(['ls', '@draftline/client', '@draftline/react', '--depth=0'], consumerRoot);

writeFileSync(
  join(consumerRoot, 'smoke.mjs'),
  `
import { createDraftlineClient } from '@draftline/client';
import { ChangedFilesList, DraftlineProvider, RemoteSyncBar } from '@draftline/react';

if (typeof createDraftlineClient !== 'function') throw new Error('client export missing');
if (typeof DraftlineProvider !== 'function') throw new Error('provider export missing');
if (typeof ChangedFilesList !== 'function') throw new Error('component export missing');
if (typeof RemoteSyncBar !== 'function') throw new Error('remote component export missing');

console.log('package consumer smoke ok');
`,
);

execFileSync('node', ['smoke.mjs'], { cwd: consumerRoot, stdio: 'inherit' });
