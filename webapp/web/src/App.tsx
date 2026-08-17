import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from './api';
import { copyText } from './clipboard';
import LeftPanel from './components/LeftPanel';
import RightPanel from './components/RightPanel';
import TopPanel from './components/TopPanel';
import WelcomePopup from './components/WelcomePopup';
import type { ExecResult, Overview, Project, OverviewVm } from './types';

interface Banner {
  kind: 'info' | 'error';
  text: string;
}

export default function App() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [selectedVmId, setSelectedVmId] = useState<string | null>(null);
  const [banner, setBanner] = useState<Banner | null>(null);
  // First visit (no localStorage flag) → show the welcome popup.
  const [welcomeOpen, setWelcomeOpen] = useState(
    () => localStorage.getItem('tikovm-welcome-seen') === null,
  );
  const closeWelcome = () => {
    localStorage.setItem('tikovm-welcome-seen', '1');
    setWelcomeOpen(false);
  };

  // 1s poll with an in-flight guard (skip overlapping requests).
  const inFlight = useRef(false);
  const refresh = useCallback(async () => {
    if (inFlight.current) {
      return;
    }
    inFlight.current = true;
    try {
      setOverview(await api.overview());
    } catch {
      // backend briefly gone; keep the last snapshot on screen
    } finally {
      inFlight.current = false;
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 1000);
    return () => clearInterval(timer);
  }, [refresh]);

  // Run a mutating action, surface its outcome, refresh immediately after.
  const run = useCallback(
    async (fn: () => Promise<unknown>, okText: string) => {
      try {
        await fn();
        setBanner({ kind: 'info', text: okText });
      } catch (err) {
        setBanner({ kind: 'error', text: err instanceof Error ? err.message : String(err) });
      } finally {
        void refresh();
      }
    },
    [refresh],
  );

  // Run an exec/SQL request; returns the result for the panel to display
  // (null on error — the banner already carries the message).
  const runQuery = useCallback(
    async (fn: () => Promise<ExecResult>, what: string): Promise<ExecResult | null> => {
      try {
        const result = await fn();
        setBanner({ kind: 'info', text: `${what} finished (exit ${result.exitCode ?? '?'})` });
        return result;
      } catch (err) {
        setBanner({ kind: 'error', text: err instanceof Error ? err.message : String(err) });
        return null;
      } finally {
        void refresh();
      }
    },
    [refresh],
  );

  const vms = overview?.vms ?? [];
  const projects = overview?.projects ?? [];
  const selectedVm: OverviewVm | undefined = vms.find((v) => v.vmId === selectedVmId);
  const selectedProject: Project | undefined = selectedVm
    ? projects.find((p) => p.id === selectedVm.projectId)
    : undefined;

  return (
    <div className="app">
      <header className="app-header">
        <span className="brand">
          tikovm <span className="x">×</span> tiko
        </span>
        <span className="header-stats">
          {projects.length} project{projects.length === 1 ? '' : 's'} · {vms.length} VM
          {vms.length === 1 ? '' : 's'}
        </span>
        <span className={`hostd ${overview ? (overview.hostdReachable ? 'ok' : 'err') : ''}`}>
          hostd {overview ? (overview.hostdReachable ? '●' : '○') : '…'}
        </span>
        <button
          className="info-btn"
          onClick={() => setWelcomeOpen(true)}
          title="about this demo"
        >
          ⓘ
        </button>
      </header>
      <main className="main">
        <TopPanel vms={vms} selectedVmId={selectedVmId} onSelect={setSelectedVmId} />
        <LeftPanel
          projects={projects}
          vmStates={Object.fromEntries(vms.map((v) => [v.vmId, v.state]))}
          selectedVmId={selectedVmId}
          onSelect={setSelectedVmId}
          onCreateProject={(name) => run(() => api.createProject(name), 'project creation started')}
          onDeleteProject={(id) => run(() => api.deleteProject(id), `project ${id} deleted`)}
          onCreateVm={(projectId, name, image) => {
            if (image.startsWith('lambda:')) {
              return run(
                () =>
                  api.createLambda(projectId, {
                    name: name || image.slice('lambda:'.length),
                    language: image.slice('lambda:'.length),
                  }),
                'lambda deploy started — follow it in the project list',
              );
            }
            if (image === 'postgrest') {
              return run(
                () => api.createPostgrest(projectId, name || 'postgrest'),
                'postgrest deploy started — follow it in the project list',
              );
            }
            return run(() => api.createVm(projectId, { name, image }), 'VM creation started');
          }}
        />
        {selectedVm ? (
          <RightPanel
            key={selectedVm.vmId}
            vm={selectedVm}
            project={selectedProject}
            onDelete={() => {
              setSelectedVmId(null);
              void run(() => api.deleteVm(selectedVm.vmId), 'VM deleted');
            }}
            onExec={(cmd) =>
              runQuery(() => api.exec(selectedVm.vmId, cmd), 'exec')
            }
            onSql={(sql) => runQuery(() => api.sql(selectedVm.vmId, sql), 'query')}
            onBranch={(name) =>
              void run(
                () => api.branch(selectedVm.vmId, name),
                'branch creation started — follow the new project in the list',
              )
            }
            onLoadLambda={() =>
              api.getLambda(selectedVm.vmId).catch((err) => {
                setBanner({
                  kind: 'error',
                  text: err instanceof Error ? err.message : String(err),
                });
                return null;
              })
            }
            onSaveLambda={async (source) => {
              try {
                await api.saveLambda(selectedVm.vmId, source);
                setBanner({ kind: 'info', text: 'lambda saved — live on the next invoke' });
                return true;
              } catch (err) {
                setBanner({
                  kind: 'error',
                  text: err instanceof Error ? err.message : String(err),
                });
                return false;
              } finally {
                void refresh();
              }
            }}
            onInvokeLambda={async (body) => {
              const slug = selectedVm.lambda?.slug;
              if (!slug) {
                return null;
              }
              try {
                const result = await api.invokeLambda(slug, body);
                setBanner({
                  kind: 'info',
                  text: `lambda ${slug} → ${result.status} in ${result.durationMs}ms`,
                });
                return result;
              } catch (err) {
                setBanner({
                  kind: 'error',
                  text: err instanceof Error ? err.message : String(err),
                });
                return null;
              } finally {
                void refresh();
              }
            }}
            onSmokePostgrest={async () => {
              const slug = selectedVm.postgrest?.slug;
              if (!slug) {
                return null;
              }
              try {
                const result = await api.smokePostgrest(slug);
                setBanner({
                  kind: 'info',
                  text: `postgrest ${slug} → ${result.status} in ${result.durationMs}ms`,
                });
                return result;
              } catch (err) {
                setBanner({
                  kind: 'error',
                  text: err instanceof Error ? err.message : String(err),
                });
                return null;
              } finally {
                void refresh();
              }
            }}
            onCopyConnStr={async () => {
              try {
                const { connectionString, expiresAt } = await api.connectionString(
                  selectedVm.vmId,
                );
                await copyText(connectionString);
                const expiry = new Date(expiresAt).toLocaleTimeString();
                setBanner({
                  kind: 'info',
                  text: `psql connection string copied to clipboard — valid until ${expiry} (1 hour)`,
                });
                return true;
              } catch (err) {
                setBanner({
                  kind: 'error',
                  text: err instanceof Error ? err.message : String(err),
                });
                return false;
              }
            }}
          />
        ) : (
          <section className="panel right">
            <div className="panel-title">VM</div>
            <div className="empty">select a VM from the project list or the table above</div>
          </section>
        )}
      </main>
      {banner && (
        <div
          className={`banner ${banner.kind}`}
          onClick={() => setBanner(null)}
          title="click to dismiss"
        >
          {banner.text}
        </div>
      )}
      {welcomeOpen && <WelcomePopup onClose={closeWelcome} />}
    </div>
  );
}
