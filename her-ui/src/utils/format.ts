import { ApiRequestError } from '../api/client';

export function formatErrorMessage(err: unknown, fallback = '操作失败'): string {
  if (err instanceof ApiRequestError) {
    return err.message || `${fallback} (HTTP ${err.status})`;
  }
  if (err instanceof Error && err.message) {
    return err.message;
  }
  return fallback;
}

export function formatDateTimeMs(ms?: number | null): string {
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms <= 0) {
    return '—';
  }
  try {
    return new Date(ms).toLocaleString('zh-CN', {
      hour12: false,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
  } catch {
    return String(ms);
  }
}

export function shortDigest(hex?: string | null): string {
  if (!hex) return '—';
  return hex.length > 14 ? `${hex.slice(0, 12)}…` : hex;
}
