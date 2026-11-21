import { ApiRequestError, apiRequest } from './client';

export interface BrowserSessionEntry {
  id: string;
  origin: string;
  createdMs: number;
  lastUsedMs: number;
  expiresMs: number;
  current: boolean;
}

export interface WhitelistGroupList {
  private?: string[];
  privates?: string[];
  groups?: string[];
  group?: string[];
}

export interface WhitelistResponse {
  allow: WhitelistGroupList;
  deny: WhitelistGroupList;
  ownerQQ: string | number | null;
}

export interface SecurityStateResponse {
  security: {
    interceptNotify: boolean;
    outboundGuard?: boolean;
    requirePasswordLocally?: boolean;
  };
  stateDir: {
    hardened: boolean;
    path?: string;
    manualCommand?: string;
    error?: string;
  };
}

export interface MemoryAclRow {
  id: number;
  reader_type: string;
  reader_id: string;
  read_type: string;
  read_id: string;
  level: string;
  created_at: number;
}

export interface AuditFileDiff {
  file: string;
  backup: string;
  beforeSha: string;
  afterSha: string;
  afterRegistryFreeSha?: string;
}

export interface StandardAuditEntry {
  id: string;
  ts: number;
  method: string;
  path: string;
  status: number;
  files: AuditFileDiff[];
  ok?: boolean;
  error?: string;
  key?: string;
}

export interface RestoreAuditEntry {
  at: number;
  action: 'backup.restore';
  by: string;
  archive: string;
  scope: 'everything' | 'memory' | string;
  result: string;
  rollbackStatus: string | null;
  preRestoreArchive: string | null;
  detail: string;
}

export type AuditEntry = StandardAuditEntry | RestoreAuditEntry;

export function isRestoreAuditEntry(entry: AuditEntry): entry is RestoreAuditEntry {
  return 'action' in entry && entry.action === 'backup.restore';
}

export interface PausedTurnSummary {
  turnId: string;
  code?: string;
  key: string;
  reason: string;
  detail?: string | null;
}

export interface TurnRow {
  turnId: string;
  code?: string;
  key: string;
  backend?: string;
  reason?: string;
  state: string;
  pauseReason?: string | null;
  pauseDetail?: string | null;
  pausedAt?: number | null;
  beganAt?: number;
  endedAt?: number | null;
  sends?: {
    batchId: string;
    total: number;
    sent: number;
    unknown: number[];
    pending: number[];
  }[];
}

export interface TurnsResponse {
  turns: TurnRow[];
  paused: PausedTurnSummary[];
  pausedCount: number;
}

export interface InboundGapRow {
  id: number;
  conversation: string;
  fromMs: number;
  toMs: number;
  status: string;
  reason?: string;
  detail?: string;
  recoveredCount: number;
  openedMs: number;
  updatedMs: number;
  attempts: number;
}

export interface InboundGapsResponse {
  stages: Record<string, number>;
  duplicates: number;
  interrupted: string[];
  unresolvedGapCount: number;
  unresolvedGaps: InboundGapRow[];
  gaps: InboundGapRow[];
}

export interface RecoveryRecord {
  v: number;
  seq: number;
  ts: number;
  runId: string;
  trigger: string;
  component: string;
  event: string;
  outcome: 'ok' | 'failed' | 'unknown' | 'skipped' | string;
  reason: string;
  repeat?: number;
  firstTs?: number;
  lastTs?: number;
}

export function changePassword(
  currentPassword: string,
  newPassword: string,
): Promise<{ ok: boolean; sessionRotated: boolean }> {
  return apiRequest('/api/auth/change-password', {
    method: 'POST',
    body: { currentPassword, newPassword },
    skipUnauthorizedRedirect: true,
  });
}

export async function fetchBrowserSessions(): Promise<{
  ok: boolean;
  sessions: BrowserSessionEntry[];
}> {
  const res = await apiRequest<{ ok: boolean; sessions?: BrowserSessionEntry[] }>(
    '/api/auth/sessions',
    { method: 'GET' },
  );
  return { ok: Boolean(res.ok), sessions: Array.isArray(res.sessions) ? res.sessions : [] };
}

export function revokeBrowserSession(
  target: { id: string } | { others: true } | { all: true },
): Promise<{ ok: boolean; revoked: number }> {
  return apiRequest('/api/auth/sessions/revoke', { method: 'POST', body: target });
}

export function fetchWhitelist(): Promise<WhitelistResponse> {
  return apiRequest<WhitelistResponse>('/api/whitelist', {
    method: 'GET',
    quoteNonIntegerNumbers: true,
  });
}

export function fetchSecurityState(): Promise<SecurityStateResponse> {
  return apiRequest<SecurityStateResponse>('/api/security', { method: 'GET' });
}

export function saveSecuritySwitches(payload: {
  interceptNotify?: boolean;
  outboundGuard?: boolean;
  requirePasswordLocally?: boolean;
  confirm: true;
}): Promise<{ ok: boolean; security: SecurityStateResponse['security'] }> {
  return apiRequest('/api/security', { method: 'POST', body: payload });
}

export async function rehardenStateDir(): Promise<{
  ok: boolean;
  hardened: boolean;
  path: string;
}> {
  try {
    return await apiRequest('/api/security', {
      method: 'POST',
      body: { rehardenStateDir: true },
    });
  } catch (err) {
    if (err instanceof ApiRequestError && err.status === 200 && err.payload) {
      return {
        ok: Boolean(err.payload.ok),
        hardened: Boolean(err.payload.hardened),
        path: typeof err.payload.path === 'string' ? err.payload.path : '',
      };
    }
    throw err;
  }
}

export function authorizeReadCheck(
  key: string,
  token: string,
): Promise<{ ok: boolean; key: string }> {
  return apiRequest('/api/authorize/read', {
    method: 'POST',
    body: { key, token },
  });
}

export async function fetchMemoryAcl(): Promise<{ ok: boolean; acl: MemoryAclRow[] }> {
  const res = await apiRequest<{ ok: boolean; acl?: MemoryAclRow[] }>('/api/memory/acl', {
    method: 'GET',
  });
  return { ok: Boolean(res.ok), acl: Array.isArray(res.acl) ? res.acl : [] };
}

export function grantMemoryAcl(payload: {
  readerType: string;
  readerId: string;
  readType: string;
  readId: string;
  level: string;
}): Promise<{ ok: boolean; id: number; acl: MemoryAclRow[] }> {
  return apiRequest('/api/memory/acl', {
    method: 'POST',
    body: { action: 'grant', ...payload },
  });
}

export function revokeMemoryAcl(payload: {
  readerType: string;
  readerId: string;
  readType: string;
  readId: string;
}): Promise<{ ok: boolean; removed: boolean; acl: MemoryAclRow[] }> {
  return apiRequest('/api/memory/acl', {
    method: 'POST',
    body: { action: 'revoke', ...payload },
  });
}

export function rotateConsoleToken(
  token?: string,
): Promise<{ ok: boolean; token: string; generated: boolean }> {
  return apiRequest('/api/console/token', {
    method: 'POST',
    body: token && token.trim() ? { token: token.trim() } : {},
  });
}

export async function fetchAuditRecent(limit = 50): Promise<{
  ok: boolean;
  count: number;
  entries: AuditEntry[];
}> {
  const res = await apiRequest<{ ok: boolean; count?: number; entries?: AuditEntry[] }>(
    `/api/audit/recent?limit=${limit}`,
    { method: 'GET' },
  );
  return {
    ok: Boolean(res.ok),
    count: res.count ?? 0,
    entries: Array.isArray(res.entries) ? res.entries : [],
  };
}

export function undoAuditEntry(
  id: string,
  confirm = false,
): Promise<{ ok: boolean; id: string; restored: string[]; revoked?: string[]; settingsKept?: string[] }> {
  return apiRequest('/api/audit/undo', {
    method: 'POST',
    body: confirm ? { id, confirm: true } : { id },
  });
}

export function fetchGlobalTurns(): Promise<TurnsResponse> {
  return apiRequest<TurnsResponse>('/api/turns', { method: 'GET' });
}

export function resolveGlobalTurn(
  turnId: string,
  action: 'abandon' | 'retry',
  confirm = false,
): Promise<{ ok: boolean; turnId: string; action: string; confirmed: boolean }> {
  return apiRequest('/api/turns/resolve', {
    method: 'POST',
    body: confirm ? { turnId, action, confirm: true } : { turnId, action },
  });
}

export function fetchGlobalInboundGaps(): Promise<InboundGapsResponse> {
  return apiRequest<InboundGapsResponse>('/api/inbound/gaps', { method: 'GET' });
}

export function retryGlobalInboundGaps(
  conversation: string,
): Promise<{ ok: boolean; conversation: string; reset: number }> {
  return apiRequest('/api/inbound/gaps/retry', {
    method: 'POST',
    body: { conversation },
  });
}

export function fetchRecoveryRecords(params?: {
  limit?: number;
  before?: number | null;
  component?: string;
  outcome?: string;
}): Promise<{ ok: boolean; records: RecoveryRecord[]; nextBefore: number | null; skipped: number }> {
  const qs = new URLSearchParams();
  if (params?.limit) qs.set('limit', String(params.limit));
  if (params?.before !== undefined && params?.before !== null) qs.set('before', String(params.before));
  if (params?.component) qs.set('component', params.component);
  if (params?.outcome) qs.set('outcome', params.outcome);
  const suffix = qs.toString() ? `?${qs.toString()}` : '';
  return apiRequest(`/api/recovery-record${suffix}`, { method: 'GET' });
}
