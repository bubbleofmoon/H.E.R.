import { ApiRequestError } from '../api/client';

export interface RuntimeZoneInfo {
  name?: string | null;
  offsetSeconds?: number;
  label?: string;
}

export function formatErrorMessage(err: unknown, fallback = '操作失败'): string {
  if (err instanceof ApiRequestError) {
    return err.message || `${fallback} (HTTP ${err.status})`;
  }
  if (err instanceof Error && err.message) {
    return err.message;
  }
  return fallback;
}

export function formatRuntimeTime(
  ms: number | null | undefined,
  zone?: RuntimeZoneInfo | null,
): string {
  if (!ms || !Number.isFinite(ms)) return '—';
  const date = new Date(ms);
  if (zone?.name) {
    try {
      return new Intl.DateTimeFormat('zh-CN', {
        timeZone: zone.name,
        hour12: false,
        year: 'numeric',
        month: '2-digit',
        day: '2-digit',
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
      }).format(date);
    } catch {
      // Fall through to fixed offset formatting when IANA name is unrecognized
    }
  }
  if (typeof zone?.offsetSeconds === 'number' && Number.isFinite(zone.offsetSeconds)) {
    const shifted = new Date(ms + zone.offsetSeconds * 1000);
    return new Intl.DateTimeFormat('zh-CN', {
      timeZone: 'UTC',
      hour12: false,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    }).format(shifted);
  }
  return date.toLocaleString('zh-CN', { hour12: false });
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

export function unicodeScalarLength(text: string): number {
  return Array.from(text).length;
}
