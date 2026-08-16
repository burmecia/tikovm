// Unit tests for lambda slug derivation and the registry's lambda lookups
// (pure logic, no hostd).

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import { Registry, toSlug } from '../src/state.js';
import type { LambdaLanguage, ProjectVmEntry } from '../src/state.js';

function lambdaEntry(vmId: string, slug: string, language: LambdaLanguage): ProjectVmEntry {
  return {
    vmId,
    name: slug,
    image: language === 'node' ? 'node-22' : 'python-3.12',
    kind: 'lambda',
    lambda: { slug, language, status: 'ready', step: '', error: undefined, source: '' },
  };
}

describe('toSlug', () => {
  it('lowercases and collapses non-alphanumerics to dashes', () => {
    assert.equal(toSlug('Hello World'), 'hello-world');
    assert.equal(toSlug('  my__fn!!  '), 'my-fn');
    assert.equal(toSlug('read-users-2'), 'read-users-2');
  });

  it('rejects names with no usable characters', () => {
    assert.equal(toSlug('!!!'), null);
    assert.equal(toSlug('---'), null);
    assert.equal(toSlug(''), null);
  });

  it('caps at 63 characters and never ends on a dash', () => {
    const slug = toSlug(`f${'a'.repeat(80)}`);
    assert.ok(slug && slug.length <= 63);
    assert.equal(toSlug(`${'a'.repeat(62)}-`), 'a'.repeat(62));
    assert.equal(toSlug(`${'a'.repeat(62)}-bbb`), 'a'.repeat(62));
  });
});

describe('Registry lambda lookups', () => {
  it('finds lambdas by slug across projects and detects collisions', () => {
    const registry = new Registry();
    const a = registry.newProject('a', 60_000);
    const b = registry.newProject('b', 60_000);
    const entry = lambdaEntry('vm-l1', 'read-db', 'node');
    a.vms.push(entry);
    b.vms.push({ vmId: 'vm-b1', name: 'web', image: 'node-22', kind: 'extra' });

    const found = registry.lambdaBySlug('read-db');
    assert.equal(found?.project, a);
    assert.equal(found?.vm, entry);
    assert.equal(registry.slugTaken('read-db'), true);
    assert.equal(registry.slugTaken('other'), false);
    assert.equal(registry.lambdaBySlug('other'), undefined);
  });
});
