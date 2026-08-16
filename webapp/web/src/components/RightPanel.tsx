import { useEffect, useState } from 'react';
import { formatAge } from '../format';
import type {
  ExecResult,
  LambdaDetail,
  LambdaInvokeResult,
  OverviewVm,
  Project,
} from '../types';
import { stateClass } from './TopPanel';

interface Props {
  vm: OverviewVm;
  project: Project | undefined;
  onDelete: () => void;
  onExec: (cmd: string) => Promise<ExecResult | null>;
  onSql: (sql: string) => Promise<ExecResult | null>;
  onCopyConnStr: () => Promise<boolean>;
  onBranch: (name: string) => void;
  onLoadLambda: () => Promise<LambdaDetail | null>;
  onSaveLambda: (source: string) => Promise<boolean>;
  onInvokeLambda: (body: string) => Promise<LambdaInvokeResult | null>;
}

/** Right panel: operations on the selected VM. */
export default function RightPanel({
  vm,
  project,
  onDelete,
  onExec,
  onSql,
  onCopyConnStr,
  onBranch,
  onLoadLambda,
  onSaveLambda,
  onInvokeLambda,
}: Props) {
  const [cmd, setCmd] = useState('uname -a');
  const [sql, setSql] = useState('select version();');
  const [branchName, setBranchName] = useState('');
  const [output, setOutput] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [lambdaSrc, setLambdaSrc] = useState<string | null>(null);
  const [invokeBody, setInvokeBody] = useState('');
  const [curlCopied, setCurlCopied] = useState(false);
  const [saveState, setSaveState] = useState<'idle' | 'saving' | 'saved'>('idle');
  const [connState, setConnState] = useState<'idle' | 'copying' | 'copied'>('idle');
  const isTiko = vm.kind === 'tiko';
  const isLambda = vm.kind === 'lambda';
  const lambda = vm.lambda;

  // Load the deployed handler source once per selected lambda VM.
  useEffect(() => {
    if (isLambda) {
      void onLoadLambda().then((d) => {
        if (d) {
          setLambdaSrc(d.source);
        }
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isLambda]);

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
        <span>{vm.name}</span>
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
          </>
        )}
      </div>

      {isLambda && lambda && (
        <>
          <div className="section-title">lambda function ({lambda.language})</div>
          {lambda.status !== 'ready' && (
            <div className={lambda.status === 'error' ? 'project-error' : 'hint dim'}>
              {lambda.status === 'error'
                ? `deploy failed: ${lambda.error ?? 'unknown error'}`
                : `deploying: ${lambda.step || 'queued'}…`}
            </div>
          )}
          <div className="button-row">
            <button
              className={curlCopied ? 'copied' : ''}
              onClick={() => {
                const url = `${window.location.origin}/api/demo/f/${lambda.slug}`;
                void navigator.clipboard.writeText(
                  `curl -X POST ${url} -d '{"hello":"world"}'`,
                );
                setCurlCopied(true);
                setTimeout(() => setCurlCopied(false), 1500);
              }}
              title="copy a curl command that invokes the lambda from anywhere"
            >
              {curlCopied ? 'copied ✓' : 'copy curl command'}
            </button>
          </div>
          <div className="hint mono dim">
            {window.location.origin}/api/demo/f/{lambda.slug}
          </div>
          <textarea
            className="mono lambda-src"
            rows={14}
            value={lambdaSrc ?? '// loading…'}
            onChange={(e) => setLambdaSrc(e.target.value)}
            disabled={lambda.status !== 'ready' || lambdaSrc === null}
            spellCheck={false}
          />
          <div className="button-row">
            <button
              className={saveState === 'saved' ? 'copied' : ''}
              disabled={
                saveState === 'saving' || lambdaSrc === null || lambda.status !== 'ready'
              }
              onClick={() => {
                setSaveState('saving');
                void onSaveLambda(lambdaSrc ?? '').then((ok) => {
                  setSaveState(ok ? 'saved' : 'idle');
                  if (ok) {
                    setTimeout(() => setSaveState('idle'), 1500);
                  }
                });
              }}
              title="syntax-checked in the guest; live on the next invoke"
            >
              {saveState === 'saving'
                ? 'deploying…'
                : saveState === 'saved'
                  ? 'saved ✓'
                  : 'save & deploy'}
            </button>
          </div>
          <form
            className="exec-form"
            onSubmit={(e) => {
              e.preventDefault();
              if (busy || lambda.status !== 'ready') {
                return;
              }
              setBusy(true);
              void onInvokeLambda(invokeBody)
                .then((r) => {
                  if (r) {
                    setOutput(
                      `POST /api/demo/f/${lambda.slug}\n→ ${r.status} in ${r.durationMs}ms\n${r.body}`,
                    );
                  }
                })
                .finally(() => setBusy(false));
            }}
          >
            <input
              className="mono"
              value={invokeBody}
              onChange={(e) => setInvokeBody(e.target.value)}
              placeholder="optional request body"
            />
            <button type="submit" disabled={busy || lambda.status !== 'ready'}>
              invoke
            </button>
          </form>
          <div className="hint dim">
            the VM auto-suspends after 2 minutes without a request; the next
            invoke wakes it (cold start — watch the duration)
          </div>
        </>
      )}

      {isTiko && (
        <>
          <div className="section-title">connect</div>
          <div className="button-row">
            <button
              className={connState === 'copied' ? 'copied' : ''}
              disabled={connState !== 'idle'}
              onClick={() => {
                setConnState('copying');
                void onCopyConnStr().then((ok) => {
                  setConnState(ok ? 'copied' : 'idle');
                  if (ok) {
                    setTimeout(() => setConnState('idle'), 1500);
                  }
                });
              }}
            >
              {connState === 'copying'
                ? 'copying…'
                : connState === 'copied'
                  ? 'copied ✓'
                  : 'copy psql connection string'}
            </button>
          </div>
          <div className="hint dim">
            mints a 1-hour proxy token and copies a psql command that connects
            through hostd's TCP proxy
          </div>

          <div className="section-title">database branching</div>
          <form
            className="exec-form"
            onSubmit={(e) => {
              e.preventDefault();
              if (project?.status === 'ready') {
                onBranch(branchName.trim());
                setBranchName('');
              }
            }}
          >
            <input
              value={branchName}
              onChange={(e) => setBranchName(e.target.value)}
              placeholder={project ? `${project.name}-branch` : 'branch project name'}
              disabled={project?.status !== 'ready'}
            />
            <button type="submit" disabled={project?.status !== 'ready'}>
              create branch
            </button>
          </form>
          <div className="hint dim">
            backs up this database and boots a new project whose database is a
            copy-on-write branch of it; deleting or expiring the source
            cascades to its branches
          </div>
        </>
      )}

      {isTiko ? (
        <div className="hint dim">
          auto-suspends when idle and wakes on the next exec or SQL request;
          deleted together with its project
        </div>
      ) : (
        <>
          <div className="section-title">actions</div>
          <div className="button-row">
            <button className="danger" onClick={onDelete}>
              delete VM
            </button>
          </div>
        </>
      )}

      {!isTiko && !isLambda && (
        <>
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
        </>
      )}

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
            psql as postgres on 127.0.0.1:5432 inside the VM; a suspended VM
            is woken automatically
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
