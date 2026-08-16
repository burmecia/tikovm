// Unit tests for the per-project tiko.env builder (pure string logic; the
// round-trip cases shell out to bash to prove the generated commands really
// reproduce the file byte-for-byte — no hostd / guest involved).

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { describe, it } from 'node:test';

import {
  SEED_DB_ID,
  SEED_PACK_PATH,
  TIKO_ENV_PATH,
  branchBackupArgv,
  branchPackPath,
  branchRestoreArgv,
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

describe('branchPackPath', () => {
  it('names the pack by the branch db id, next to the seed pack', () => {
    assert.equal(branchPackPath(1003), '/mnt/s3files/tiko_backup/branch-1003.tar.zst');
  });
});

describe('branchBackupArgv', () => {
  it('backs up the local postgres into the given pack', () => {
    assert.deepEqual(branchBackupArgv('/mnt/s3files/tiko_backup/branch-1003.tar.zst'), [
      'tiko_branch',
      'backup',
      '--pack',
      '/mnt/s3files/tiko_backup/branch-1003.tar.zst',
      '--host',
      '127.0.0.1',
      '--port',
      '5432',
      '--user',
      'postgres',
    ]);
  });
});

describe('branchRestoreArgv', () => {
  it('branches the given db/project from the seed pack (db_id=0)', () => {
    assert.deepEqual(
      branchRestoreArgv({
        packPath: SEED_PACK_PATH,
        parentDbId: SEED_DB_ID,
        dbId: 1001,
        projectId: 1000,
      }),
      [
        'tiko_branch',
        'restore',
        '--pack',
        '/mnt/s3files/tiko_backup/0.tar.zst',
        '--parent-db-id',
        '0',
        '--db-id',
        '1001',
        '--project-id',
        '1000',
        '--pgdata',
        '/var/lib/postgresql/tt',
        '--branch-port',
        '5432',
        '--recovery-timeout',
        '240',
      ],
    );
  });

  it('branches from another project’s pack and db id', () => {
    const argv = branchRestoreArgv({
      packPath: branchPackPath(1003),
      parentDbId: 1001,
      dbId: 1003,
      projectId: 1002,
    });
    assert.deepEqual(argv.slice(0, 8), [
      'tiko_branch',
      'restore',
      '--pack',
      '/mnt/s3files/tiko_backup/branch-1003.tar.zst',
      '--parent-db-id',
      '1001',
      '--db-id',
      '1003',
    ]);
  });
});

describe('shellQuote', () => {
  it('escapes embedded single quotes', () => {
    assert.equal(shellQuote("a'b"), `'a'\\''b'`);
  });
});
