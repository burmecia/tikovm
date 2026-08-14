import { formatAge, formatMiB } from '../format';
import type { OverviewVm } from '../types';

/** State string → css badge class (colors defined in style.css). */
export function stateClass(state: string): string {
  switch (state) {
    case 'started':
      return 'st-started';
    case 'paused':
      return 'st-paused';
    case 'suspended':
      return 'st-suspended';
    case 'destroyed':
      return 'st-destroyed';
    default:
      // creating/starting/pausing/resuming/suspending/restoring/destroying
      return 'st-transitional';
  }
}

interface Props {
  vms: OverviewVm[];
  selectedVmId: string | null;
  onSelect: (vmId: string) => void;
}

/** Top panel: vmtop-style live inventory of all demo VMs. */
export default function TopPanel({ vms, selectedVmId, onSelect }: Props) {
  return (
    <section className="panel top">
      <div className="panel-title">
        VMs <span className="panel-count">{vms.length}</span>
      </div>
      <div className="table-scroll">
        <table className="vm-table">
          <thead>
            <tr>
              <th>STATE</th>
              <th>VM</th>
              <th>NAME</th>
              <th>PROJECT</th>
              <th>KIND</th>
              <th>IMAGE</th>
              <th>IP</th>
              <th>CPU</th>
              <th>MEM</th>
              <th>AGE</th>
            </tr>
          </thead>
          <tbody>
            {vms.length === 0 && (
              <tr>
                <td colSpan={10} className="empty">
                  no VMs — create a project to boot a tiko postgres VM
                </td>
              </tr>
            )}
            {vms.map((vm) => (
              <tr
                key={vm.vmId}
                className={vm.vmId === selectedVmId ? 'selected' : ''}
                onClick={() => onSelect(vm.vmId)}
              >
                <td>
                  <span className={`badge ${stateClass(vm.state)}`}>{vm.state}</span>
                </td>
                <td className="mono">{vm.vmId}</td>
                <td>{vm.name}</td>
                <td className="mono">#{vm.projectId}</td>
                <td>
                  <span className={`kind ${vm.kind}`}>{vm.kind}</span>
                </td>
                <td>{vm.image}</td>
                <td className="mono">{vm.guestIp ?? '—'}</td>
                <td className="num">{vm.cpus}</td>
                <td className="num">{formatMiB(vm.memoryMb)}</td>
                <td className="num">{formatAge(vm.createdAt)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
