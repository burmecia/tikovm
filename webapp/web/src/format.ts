// Terse duration/size formatting helpers (vmtop style).

/** Age since an ISO timestamp, e.g. `42s`, `3m12s`, `1h04m`. */
export function formatAge(iso: string, now = Date.now()): string {
  const secs = Math.max(0, Math.floor((now - Date.parse(iso)) / 1000));
  return formatDuration(secs);
}

/** A duration in seconds, e.g. `42s`, `3m12s`, `1h04m`. */
export function formatDuration(secs: number): string {
  if (secs < 60) {
    return `${secs}s`;
  }
  if (secs < 3600) {
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return s > 0 ? `${m}m${s}s` : `${m}m`;
  }
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return m > 0 ? `${h}h${String(m).padStart(2, '0')}m` : `${h}h`;
}

/** Countdown in seconds as `mm:ss` (or `h:mm:ss` past an hour). */
export function formatCountdown(totalSecs: number): string {
  const s = Math.max(0, totalSecs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`;
  }
  return `${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`;
}

/** MiB as a compact human size. */
export function formatMiB(mb: number): string {
  return mb >= 1024 ? `${(mb / 1024).toFixed(mb % 1024 === 0 ? 0 : 1)}G` : `${mb}M`;
}
