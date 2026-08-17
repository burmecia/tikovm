/** Welcome modal: shown on first visit, re-openable from the header's info
 * button. Information-only — no interaction besides closing. */
export default function WelcomePopup({ onClose }: { onClose: () => void }) {
  return (
    <div className="welcome-overlay" onClick={onClose}>
      {/* Clicks inside the dialog must not close it. */}
      <div className="welcome-dialog" onClick={(e) => e.stopPropagation()}>
        <button className="welcome-close" onClick={onClose} title="close">
          ✕
        </button>
        <div className="welcome-title">welcome to tikovm × tiko</div>
        <p>
          This demo drives <strong>tikovm</strong> (Firecracker microVM
          management) and <strong>tiko</strong> (Postgres with S3-backed,
          copy-on-write storage) end to end — every project you create boots
          real microVMs on demand.
        </p>
        <p>How to use it:</p>
        <ul>
          <li>create a project — this boots a tiko postgres VM</li>
          <li>
            select a VM — SQL console, psql connection string, database
            branching for the database VM
          </li>
          <li>
            add more VMs to a project with "+ VM": ubuntu-24, lambda functions
            (node-22 / python-3.12), or a postgREST API over the database
          </li>
          <li>
            idle VMs auto-suspend (zero CPU/memory) and wake on the next
            request
          </li>
        </ul>
        <p>
          Every project has a hard <strong>1-hour TTL</strong>: projects, VMs
          and all their data are deleted when it expires.
        </p>
        <p>
          This demo runs on a 2 vCPU / 4 GiB server — be gentle 🙂
        </p>
        <p className="welcome-links">
          <a href="https://github.com/burmecia/tiko" target="_blank" rel="noreferrer">
            github: tiko
          </a>
          {' · '}
          <a href="https://github.com/burmecia/tikovm" target="_blank" rel="noreferrer">
            github: tikovm
          </a>
        </p>
      </div>
    </div>
  );
}
