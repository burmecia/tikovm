// Unit tests for the in-memory project registry (pure logic, no hostd).

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import { IdAllocator, Registry } from '../src/state.js';

describe('IdAllocator', () => {
  it('allocates monotonically increasing ids', () => {
    const ids = new IdAllocator(1000);
    assert.equal(ids.alloc(), 1000);
    assert.equal(ids.alloc(), 1001);
    assert.equal(ids.alloc(), 1002);
  });
});

describe('Registry', () => {
  it('project ids and db ids are unique and never equal', () => {
    const registry = new Registry();
    const a = registry.newProject('a', 60_000);
    const b = registry.newProject('b', 60_000);
    const all = [a.id, a.dbId, b.id, b.dbId];
    assert.equal(new Set(all).size, all.length, 'ids must not collide');
    assert.notEqual(a.id, a.dbId);
    assert.notEqual(a.dbId, b.id);
  });

  it('lists projects in creation order and finds vm owners', () => {
    const registry = new Registry();
    const a = registry.newProject('a', 60_000);
    const b = registry.newProject('b', 60_000);
    a.vms.push({ vmId: 'vm-a1', name: 'tiko-pg', image: 'tiko-postgres', kind: 'tiko' });
    b.vms.push({ vmId: 'vm-b1', name: 'web', image: 'node-22', kind: 'extra' });

    assert.deepEqual(registry.list().map((p) => p.name), ['a', 'b']);
    assert.equal(registry.vmOwner('vm-a1'), a);
    assert.equal(registry.vmOwner('vm-b1'), b);
    assert.equal(registry.vmOwner('vm-zz'), undefined);
    assert.deepEqual(registry.allVmIds(), ['vm-a1', 'vm-b1']);
  });

  it('expires only projects past their TTL that are not deleting', () => {
    const t0 = 1_000_000;
    const registry = new Registry();
    registry.newProject('fresh', 60_000, t0);
    registry.newProject('stale', 10_000, t0);
    const deleting = registry.newProject('deleting', 10_000, t0);
    deleting.status = 'deleting';
    // 60s later: fresh (ttl 60s) is exactly at expiry, stale long past.
    const expired = registry.expired(t0 + 60_000).map((p) => p.name);
    assert.deepEqual(expired, ['fresh', 'stale']);
    assert.ok(!expired.includes('deleting'));
  });

  it('removeVm detaches a VM from its project only', () => {
    const registry = new Registry();
    const a = registry.newProject('a', 60_000);
    a.vms.push({ vmId: 'vm-a1', name: 'x', image: 'ubuntu-24', kind: 'extra' });
    a.vms.push({ vmId: 'vm-a2', name: 'y', image: 'ubuntu-24', kind: 'extra' });
    registry.removeVm('vm-a1');
    assert.deepEqual(a.vms.map((v) => v.vmId), ['vm-a2']);
    assert.equal(registry.vmOwner('vm-a1'), undefined);
  });
});
