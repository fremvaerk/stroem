/**
 * Formats a date string as a short locale string: "Feb 27, 14:30"
 * Accepts null to return "-".
 */
export function formatTime(dateStr: string | null): string {
  if (!dateStr) return "-";
  return new Date(dateStr).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/**
 * Formats the elapsed time between a start and end timestamp.
 * If end is null, uses the current time (live duration).
 * Returns "-" when start is null.
 * Examples: "5s", "2m 30s", "1h 5m"
 */
export function formatDuration(
  start: string | null,
  end: string | null,
): string {
  if (!start) return "-";
  const s = new Date(start).getTime();
  const e = end ? new Date(end).getTime() : Date.now();
  const diff = Math.max(0, e - s);
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const secs = seconds % 60;
  if (minutes < 60) return `${minutes}m ${secs}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

/**
 * Formats a date string as a human-readable relative time: "5s ago", "2m ago", "3h ago", "1d ago".
 * Returns "Never" when dateStr is null.
 */
export function formatRelativeTime(dateStr: string | null): string {
  if (!dateStr) return "Never";
  const diff = Date.now() - new Date(dateStr).getTime();
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/**
 * Formats a future ISO date string as a human-readable countdown: "in 5m", "in 3h 10m", "in 2d 4h".
 * Returns "past" when the date is in the past.
 */
export function formatFutureTime(isoStr: string): string {
  const diff = new Date(isoStr).getTime() - Date.now();
  if (diff < 0) return "past";
  const minutes = Math.floor(diff / 60000);
  if (minutes < 60) return `in ${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `in ${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `in ${days}d ${hours % 24}h`;
}
