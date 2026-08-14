import { useState } from 'react';
import { formatAge } from '../format';
import type { ExecResult, OverviewVm, Project } from '../types';
import { stateClass } from './TopPanel';

interface Props {
  vm: OverviewVm;
  project: Project | undefined;
  onLifecycle: (action: string) => void;
  onDelete: () => void;
  onExec: (cmd: string) => Promise<ExecResult | null>;
  onSql: (sql: string) => Promise<ExecResult | null>;
}

/** Right panel: operations on the selected VM. */
export default function RightPanel({
  vm,
  project,
  onLifecycle,
  onDelete,
  onExec,
  onSql,
}: Props) {
  const [cmd, setCmd] = useState('uname -a');
  const [sql, setSql] = useState('select version();');
  const [output, setOutput] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const isTiko = vm.kind === 'tiko';

  const canPause = vm.state === 'started';
  const canResume = vm.state === 'paused';
  const canSnapshot = vm.state === 'started';
  const canRestore = vm.state === 'suspended';

  const runAndShow = async (what: string, fn: () => Promise<ExecResult | null>) => {
    setBusy(true);
    setOutput(`$ ${what}\n`);
    const result = await fn();
    setOutput(`$ ${what}\n${result ? result.output : ''}\n[exit ${result?.exitCode ?? '?'}]`);
    setBusy(false);
  };

  return (
    <section className="panel right">
      <div className="panel-title vm-title">
        <span>
          {vm.name} <span className="kind">{vm.kind}</span>
        </span>
        <span className={`badge ${stateClass(vm.state)}`}>{vm.state}</span>
      </div>

      <div className="info-grid">
        <span className="k">vm</span>
        <span className="v mono">{vm.vmId}</span>
        <span className="k">image</span>
        <span className="v">{vm.image}</span>
        <span className="k">project</span>
        <span className="v mono">
          {project ? `${project.name} #${project.id}` : `#${vm.projectId}`}
        </span>
        <span className="k">ip</span>
        <span className="v mono">{vm.guestIp ?? '—'}</span>
        <span className="k">cpu / mem</span>
        <span className="v">
          {vm.cpus} vCPU · {vm.memoryMb} MiB
        </span>
        <span className="k">age</span>
        <span className="v">{formatAge(vm.createdAt)}</span>
        {isTiko && project && (
          <>
            <span className="k">tiko identity</span>
            <span className="v mono">
              org 12 · db {project.dbId} · project {project.id}
            </span>
            <span className="k">connect</span>
            <span className="v mono connect">
              psql -h {vm.guestIp ?? '<ip>'} -U postgres -d postgres
            </span>
          </>
        )}
      </div>

      <div className="section-title">lifecycle</div>
      <div className="button-row">
        <button disabled={!canPause} onClick={() => onLifecycle('pause')}>
          pause
        </button>
        <button disabled={!canResume} onClick={() => onLifecycle('resume')}>
          resume
        </button>
        <button disabled={!canSnapshot} onClick={() => onLifecycle('snapshot')}>
          snapshot
        </button>
        <button disabled={!canRestore} onClick={() => onLifecycle('restore')}>
          restore
        </button>
        <button className="danger" onClick={onDelete}>
          delete VM
        </button>
      </div>

      <div className="section-title">exec in guest</div>
      <form
        className="exec-form"
        onSubmit={(e) => {
          e.preventDefault();
          if (cmd.trim() && !busy) {
            void runAndShow(cmd, () => onExec(cmd));
          }
        }}
      >
        <input
          className="mono"
          value={cmd}
          onChange={(e) => setCmd(e.target.value)}
          placeholder="command to run inside the VM"
        />
        <button type="submit" disabled={busy || vm.state === 'destroyed'}>
          run
        </button>
      </form>
      <div className="hint dim">
        runs as root via a login shell; a suspended VM is woken automatically
      </div>

      {isTiko && (
        <>
          <div className="section-title">sql (tiko postgres)</div>
          <form
            className="sql-form"
            onSubmit={(e) => {
              e.preventDefault();
              if (sql.trim() && !busy) {
                void runAndShow(sql, () => onSql(sql));
              }
            }}
          >
            <textarea
              className="mono"
              rows={4}
              value={sql}
              onChange={(e) => setSql(e.target.value)}
              placeholder="select …"
            />
            <button type="submit" disabled={busy}>
              run sql
            </button>
          </form>
          <div className="hint dim">
            psql as postgres on 127.0.0.1:5432 inside the VM — needs a database
            initialized first (backup/restore init, coming)
          </div>
        </>
      )}

      {output !== null && (
        <>
          <div className="section-title">output</div>
          <pre className="output mono">{output}</pre>
        </>
      )}
    </section>
  );
}
