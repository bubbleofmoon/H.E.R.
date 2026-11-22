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

export function formatDateTimeValue(val?: number | string | null): string {
  if (val === null || val === undefined || val === '') {
    return '—';
  }
  if (typeof val === 'number') {
    return formatDateTimeMs(val);
  }
  const asNum = Number(val);
  if (Number.isFinite(asNum) && asNum > 0) {
    return formatDateTimeMs(asNum);
  }
  const parsed = Date.parse(val);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return '—';
  }
  return formatDateTimeMs(parsed);
}

export function formatTokenUnit(unit?: number | string | null): string {
  if (typeof unit === 'string' && unit.trim()) return unit.trim();
  if (typeof unit === 'number' && Number.isFinite(unit)) {
    if (unit === 1_000_000) return '1M tokens';
    if (unit === 1_000) return '1k tokens';
    return `${unit.toLocaleString()} tokens`;
  }
  return '1M tokens';
}

export function buildConversationNameMap(
  conversations: { key: string; name?: string }[],
): Map<string, string> {
  const map = new Map<string, string>();
  for (const conv of conversations) {
    if (conv.name) {
      map.set(conv.key, conv.name);
    }
  }
  return map;
}

export function formatAppliesLabel(applies?: string): string {
  if (applies === 'live') return '即时生效';
  if (applies === 'next wake') return '下次唤醒生效';
  if (applies === 'restart') return '重启后生效';
  return applies || '—';
}

export function shortDigest(hex?: string | null): string {
  if (!hex) return '—';
  return hex.length > 14 ? `${hex.slice(0, 12)}…` : hex;
}
