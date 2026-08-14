// Unit tests for the per-project tiko.env builder (pure string logic; the
// round-trip cases shell out to bash to prove the generated commands really
// reproduce the file byte-for-byte — no hostd / guest involved).

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { describe, it } from 'node:test';

import {
  TIKO_ENV_PATH,
  buildTikoEnv,
  shellQuote,
  tikoEnvWriteCmd,
  tikoNamespace,
} from '../src/tikoenv.js';

const TMP = '/tmp/tikovm-webapp-tikoenv-test';

describe('buildTikoEnv', () => {
  it('renders the six identity keys with the given ids', () => {
    const env = buildTikoEnv({ orgId: 12, dbId: 1001, projectId: 1000, vmId: 'vm-1000-a1b2c3' });
    assert.equal(
      env,
      [
        'TIKO_ORG_ID=12',
        'TIKO_DB_ID=1001',
        'TIKO_PROJECT_ID=1000',
        'TIKO_STORAGE_ROOT=/mnt/s3files/tiko_root',
        'TIKO_LOCAL_PATH=/var/lib/postgresql/tiko_local',
        'TIKO_VM_ID=vm-1000-a1b2c3',
        '',
      ].join('\n'),
    );
  });

  it('is safely source-able as shell assignments', () => {
    const env = buildTikoEnv({ orgId: 12, dbId: 42, projectId: 41, vmId: 'vm-41-xyz' });
    const out = execFileSync(
      'bash',
      ['-c', `cat > ${TMP}.src && set -a && . ${TMP}.src && echo "$TIKO_DB_ID $TIKO_VM_ID"`],
      { input: env, encoding: 'utf8' },
    ).trim();
    assert.equal(out, '42 vm-41-xyz');
  });
});

describe('tikoNamespace', () => {
  it('namespaces by org/db under the S3 Files mount', () => {
    assert.equal(tikoNamespace(12, 1001), '/mnt/s3files/tiko_root/s3sim/12/1001');
  });
});

describe('tikoEnvWriteCmd', () => {
  it('round-trips content through printf > file, even with quotes in values', () => {
    const content = buildTikoEnv({ orgId: 1, dbId: 2, projectId: 3, vmId: "vm-3-quot'es" });
    const cmd = tikoEnvWriteCmd(content);
    assert.equal(cmd[0], 'bash');
    assert.equal(cmd[1], '-c');
    assert.match(cmd[2], new RegExp(`printf.*> ${TIKO_ENV_PATH.replace(/\//g, '\\/')}`));
    assert.match(cmd[2], /chown postgres:postgres/);
    // Execute the pipeline against a temp path and compare byte-for-byte.
    // The chown is dropped (we run as a normal user here; it is asserted above).
    const script = cmd[2]
      .replaceAll(TIKO_ENV_PATH, `${TMP}.out`)
      .replace(/ && chown .*/, '');
    execFileSync('bash', ['-c', script]);
    assert.equal(readFileSync(`${TMP}.out`, 'utf8'), content);
  });
});

describe('shellQuote', () => {
  it('escapes embedded single quotes', () => {
    assert.equal(shellQuote("a'b"), `'a'\\''b'`);
  });
});
