//! In-memory demo project registry — the webapp's only state store.
//!
//! A "project" is a demo unit: one tiko postgres VM (created automatically)
//! plus any number of extra VMs the user adds. Projects (and their VMs) are
//! deleted automatically when their TTL expires, and the whole registry is
//! wiped on shutdown.
//!
//! A project may also be a **database branch** of another project
//! (`branchedFrom`): its database was restored from a backup of the source
//! project and reads the source's chunks copy-on-write through the shared
//! tiko storage root (transitively, for nested branches). Projects therefore
//! form a branch tree, and deleting a project — manually, by TTL, or at
//! shutdown — must cascade to all its descendants: a branch whose ancestor's
//! S3 namespace is wiped is corrupted.
//!
//! Project ids and tiko db ids are allocated from the same monotonic counter,
//! so every id the app ever hands out is unique and no db id ever collides
//! with a project id (or any other db id — that matters because the tiko
//! storage manager namespaces S3 Files data by db id).

export type ProjectStatus = 'provisioning' | 'ready' | 'error' | 'deleting';

/** Runtime language of a lambda VM (selects the guest image + wrapper). */
export type LambdaLanguage = 'node' | 'python';

/** Deploy state of a lambda, tracked per VM like a project's status. */
export type LambdaStatus = 'deploying' | 'ready' | 'error';

/** Lambda-function metadata attached to a `kind: 'lambda'` VM entry. */
export interface LambdaMeta {
  /** URL slug the function is invoked at (`/api/demo/f/<slug>`); immutable. */
  slug: string;
  language: LambdaLanguage;
  status: LambdaStatus;
  /** Human-readable current deploy step; empty when idle. */
  step: string;
  error: string | undefined;
  /** Last successfully deployed handler source (mirror of the guest file). */
  source: string;
}

/** A VM tracked under a project. `kind: 'tiko'` marks the project's database. */
export interface ProjectVmEntry {
  vmId: string;
  name: string;
  image: string;
  kind: 'tiko' | 'extra' | 'lambda';
  /** Set for `kind: 'lambda'` entries. */
  lambda?: LambdaMeta;
}

/** Slug validity: lowercase dns-label-ish, derived from the function name. */
export const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,62}$/;

/**
 * Derive a URL slug from a function name: lowercase, non-alphanumerics
 * collapsed to dashes, edges trimmed. Returns null when nothing usable
 * remains (e.g. an all-punctuation name).
 */
export function toSlug(name: string): string | null {
  const slug = name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 63)
    .replace(/-+$/g, '');
  return SLUG_RE.test(slug) ? slug : null;
}

/** Where a branch project's database was copied from. */
export interface BranchOrigin {
  projectId: number;
  dbId: number;
}

export interface Project {
  readonly id: number;
  /** Unique tiko db id (never equal to any project id or other db id). */
  readonly dbId: number;
  name: string;
  /** Set when this project's database branched from another project's. */
  readonly branchedFrom: BranchOrigin | undefined;
  status: ProjectStatus;
  /** Human-readable current provisioning/cleanup step; empty when idle. */
  step: string;
  error: string | undefined;
  createdAt: string;
  /** Epoch milliseconds; the TTL sweeper deletes the project after this. */
  expiresAt: number;
  vms: ProjectVmEntry[];
}

/** Monotonic id allocator; one instance backs both id spaces for uniqueness. */
export class IdAllocator {
  #next: number;
  constructor(start = 1000) {
    this.#next = start;
  }
  alloc(): number {
    return this.#next++;
  }
}

export class Registry {
  #projects = new Map<number, Project>();
  #ids: IdAllocator;

  constructor(ids: IdAllocator = new IdAllocator()) {
    this.#ids = ids;
  }

  /**
   * Create and register a fresh project. The id and dbId come from the same
   * counter (unique across both spaces); the TTL clock starts now.
   * `branchedFrom` marks the project as a database branch of another project.
   */
  newProject(
    name: string,
    ttlMs: number,
    now = Date.now(),
    branchedFrom?: BranchOrigin,
  ): Project {
    const project: Project = {
      id: this.#ids.alloc(),
      dbId: this.#ids.alloc(),
      name,
      branchedFrom,
      status: 'provisioning',
      step: 'queued',
      error: undefined,
      createdAt: new Date(now).toISOString(),
      expiresAt: now + ttlMs,
      vms: [],
    };
    this.#projects.set(project.id, project);
    return project;
  }

  get(id: number): Project | undefined {
    return this.#projects.get(id);
  }

  remove(id: number): void {
    this.#projects.delete(id);
  }

  /** All projects, ordered by id (creation order). */
  list(): Project[] {
    return [...this.#projects.values()].sort((a, b) => a.id - b.id);
  }

  /** The project that owns the given hostd vm id, if any. */
  vmOwner(vmId: string): Project | undefined {
    return this.list().find((p) => p.vms.some((v) => v.vmId === vmId));
  }

  /** The VM entry owning the given lambda slug, if any (slugs are unique). */
  lambdaBySlug(
    slug: string,
  ): { project: Project; vm: ProjectVmEntry } | undefined {
    for (const p of this.#projects.values()) {
      const vm = p.vms.find((v) => v.lambda?.slug === slug);
      if (vm) {
        return { project: p, vm };
      }
    }
    return undefined;
  }

  /** Whether a lambda slug is already taken. */
  slugTaken(slug: string): boolean {
    return this.lambdaBySlug(slug) !== undefined;
  }

  /**
   * All transitive descendants of the given project (branches of it, branches
   * of those, …), ordered children-before-parents (post-order) so a cascade
   * delete never removes a namespace while one of its dependents is still
   * alive. A branch reads its ancestors' chunks copy-on-write, so deleting an
   * ancestor must cascade through this list.
   */
  descendants(id: number): Project[] {
    const out: Project[] = [];
    const visit = (parentId: number) => {
      for (const p of this.#projects.values()) {
        if (p.branchedFrom?.projectId === parentId) {
          visit(p.id);
          out.push(p);
        }
      }
    };
    visit(id);
    return out;
  }

  /** Remove a VM entry from its project (when the VM is deleted). */
  removeVm(vmId: string): void {
    for (const p of this.#projects.values()) {
      const before = p.vms.length;
      p.vms = p.vms.filter((v) => v.vmId !== vmId);
      if (p.vms.length !== before) {
        return;
      }
    }
  }

  allVmIds(): string[] {
    return this.list().flatMap((p) => p.vms.map((v) => v.vmId));
  }

  /** Projects past their TTL that are not already being deleted. */
  expired(now: number): Project[] {
    return this.list().filter(
      (p) => p.status !== 'deleting' && now >= p.expiresAt,
    );
  }
}
