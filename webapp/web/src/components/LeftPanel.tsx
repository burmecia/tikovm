import { useState } from 'react';
import { formatCountdown } from '../format';
import { EXTRA_IMAGES } from '../types';
import type { Project, ProjectStatus } from '../types';
import { stateClass } from './TopPanel';

interface Props {
  projects: Project[];
  /** vmId → live state, joined from the overview's VM list. */
  vmStates: Record<string, string>;
  selectedVmId: string | null;
  onSelect: (vmId: string) => void;
  onCreateProject: (name: string) => void;
  onDeleteProject: (id: number) => void;
  onCreateVm: (projectId: number, name: string, image: string) => void;
}

function statusBadge(status: ProjectStatus, step: string): JSX.Element {
  switch (status) {
    case 'provisioning':
      return (
        <span className="badge st-transitional" title={step}>
          provisioning·{step || 'queued'}
        </span>
      );
    case 'ready':
      return <span className="badge st-started">ready</span>;
    case 'deleting':
      return <span className="badge st-transitional">deleting</span>;
    case 'error':
      return <span className="badge st-destroyed" title="provisioning failed">error</span>;
  }
}

/** Left panel: project list (create/delete) with nested, clickable VMs. */
export default function LeftPanel({
  projects,
  vmStates,
  selectedVmId,
  onSelect,
  onCreateProject,
  onDeleteProject,
  onCreateVm,
}: Props) {
  const [newName, setNewName] = useState('');
  const [addingTo, setAddingTo] = useState<number | null>(null);
  const [vmName, setVmName] = useState('');
  const [vmImage, setVmImage] = useState<string>(EXTRA_IMAGES[0]);

  return (
    <section className="panel left">
      <div className="panel-title">Projects</div>
      <form
        className="new-project"
        onSubmit={(e) => {
          e.preventDefault();
          onCreateProject(newName.trim());
          setNewName('');
        }}
      >
        <input
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          placeholder="new project name"
        />
        <button type="submit">+ project</button>
      </form>
      <div className="project-list">
        {projects.length === 0 && (
          <div className="empty">no projects — create one to get a tiko postgres VM</div>
        )}
        {projects.map((p) => (
          <div key={p.id} className="project">
            <div className="project-head">
              <span className="project-name" title={`db id ${p.dbId}`}>
                {p.name} <span className="mono dim">#{p.id}</span>
              </span>
              <span className="mono dim" title="time until this project (and its VMs) is deleted">
                ⏳ {formatCountdown(p.expiresInSeconds)}
              </span>
            </div>
            <div className="project-meta">
              {statusBadge(p.status, p.step)}
              <span className="mono dim">db {p.dbId}</span>
              <button
                className="danger small"
                onClick={() => onDeleteProject(p.id)}
                title="delete project and all its VMs"
              >
                delete
              </button>
            </div>
            {p.status === 'provisioning' && p.step && (
              <div className="project-step mono">{p.step}…</div>
            )}
            {p.error && <div className="project-error" title={p.error}>{p.error}</div>}
            <ul className="vm-list">
              {p.vms.length === 0 && p.status !== 'provisioning' && (
                <li className="empty">no VMs</li>
              )}
              {p.vms.map((vm) => {
                const state = vmStates[vm.vmId] ?? 'unknown';
                return (
                  <li
                    key={vm.vmId}
                    className={vm.vmId === selectedVmId ? 'selected' : ''}
                    onClick={() => onSelect(vm.vmId)}
                  >
                    <span className={`dot ${stateClass(state)}`} title={state} />
                    <span className="vm-name">{vm.name}</span>
                    <span className="mono dim">{vm.image}</span>
                    <span className={`badge ${stateClass(state)}`}>{state}</span>
                  </li>
                );
              })}
            </ul>
            {p.status === 'ready' && (
              addingTo === p.id ? (
                <form
                  className="add-vm"
                  onSubmit={(e) => {
                    e.preventDefault();
                    onCreateVm(p.id, vmName.trim(), vmImage);
                    setAddingTo(null);
                    setVmName('');
                  }}
                >
                  <input
                    value={vmName}
                    onChange={(e) => setVmName(e.target.value)}
                    placeholder="vm name"
                    autoFocus
                  />
                  <select value={vmImage} onChange={(e) => setVmImage(e.target.value)}>
                    {EXTRA_IMAGES.map((img) => (
                      <option key={img} value={img}>
                        {img}
                      </option>
                    ))}
                  </select>
                  <button type="submit">add</button>
                  <button type="button" className="dim" onClick={() => setAddingTo(null)}>
                    ✕
                  </button>
                </form>
              ) : (
                <button className="small" onClick={() => setAddingTo(p.id)}>
                  + VM
                </button>
              )
            )}
          </div>
        ))}
      </div>
    </section>
  );
}
