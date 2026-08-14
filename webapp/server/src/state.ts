//! In-memory demo project registry — the webapp's only state store.
//!
//! A "project" is a demo unit: one tiko postgres VM (created automatically)
//! plus any number of extra VMs the user adds. Projects (and their VMs) are
//! deleted automatically when their TTL expires, and the whole registry is
//! wiped on shutdown.
//!
//! Project ids and tiko db ids are allocated from the same monotonic counter,
//! so every id the app ever hands out is unique and no db id ever collides
//! with a project id (or any other db id — that matters because the tiko
//! storage manager namespaces S3 Files data by db id).

export type ProjectStatus = 'provisioning' | 'ready' | 'error' | 'deleting';

/** A VM tracked under a project. `kind: 'tiko'` marks the project's database. */
export interface ProjectVmEntry {
  vmId: string;
  name: string;
  image: string;
  kind: 'tiko' | 'extra';
}

export interface Project {
  readonly id: number;
  /** Unique tiko db id (never equal to any project id or other db id). */
  readonly dbId: number;
  name: string;
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
   */
  newProject(name: string, ttlMs: number, now = Date.now()): Project {
    const project: Project = {
      id: this.#ids.alloc(),
      dbId: this.#ids.alloc(),
      name,
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
