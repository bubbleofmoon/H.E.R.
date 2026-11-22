import { ApiRequestError, SettingItemView, apiRequest, fetchSettingsList } from './client';

export interface OptionalResult<T> {
  available: boolean;
  status?: number;
  reason?: string;
  data?: T;
}

export type ScheduleKind = 'job' | 'reminder' | 'sleep' | 'replyCheck' | 'reachOut';
export type ScheduleActionName = 'pause' | 'resume' | 'edit' | 'cancel' | 'run';

export interface ScheduleLastResult {
  at: number;
  summary: string;
  late: boolean;
}

export interface ScheduleEntry {
  id: string;
  kind: ScheduleKind;
  creator: 'system' | 'owner' | 'her';
  conversation: string | null;
  nextRunAt: number | null;
  repeat: string;
  action: string;
  status: 'running' | 'paused' | 'disabled' | 'active' | 'overdue' | 'stale' | string;
  createdAt: number | null;
  lastResult: ScheduleLastResult | null;
  actions: ScheduleActionName[];
  observedAt?: number;
}

export interface ScheduleListResponse {
  ok: boolean;
  now: number;
  timeZone: {
    name: string | null;
    offsetSeconds: number;
    label: string;
  };
  entries: ScheduleEntry[];
}

export type ScheduleRepeat =
  | { kind: 'once' }
  | { kind: 'every'; minutes: number }
  | { kind: 'daily'; hour: number; minute: number };

export interface ScheduleHistoryRow {
  at: number;
  id: string;
  conversation: string;
  summary: string;
  late: boolean;
}

export type TaskState = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'unknown';

export interface RunView {
  runId: string;
  n: number;
  taskId: string;
  source: 'scheduled' | 'manual' | 'recovery' | 'login' | string;
  state: TaskState;
  queuedAt: number;
  startedAt: number | null;
  endedAt: number | null;
  exitCode: number | null;
  reason: string | null;
  pid: number | null;
  requestId: string | null;
  by: string | null;
}

export interface TaskRow {
  id: string;
  title: string;
  effect: string;
  schedule: string;
  enabled: boolean;
  nextRunAt: number | null;
  replaces: string | null;
  running: boolean;
  lastRun: RunView | null;
}

export interface LegacyTaskRow {
  name: string;
  disposition: 'migrated' | 'retired' | string;
  replacement: string | null;
  note: string;
}

export interface DesiredRecord {
  v: number;
  state: 'running' | 'stopped' | 'maintenance';
  by: string;
  reason: string;
  at: number;
  until?: number;
  revision: number;
}

export type DesiredView =
  | {
      state: 'running' | 'stopped' | 'maintenance';
      wants: { live: boolean; console: boolean; jobs: boolean };
      record: DesiredRecord | null;
    }
  | { state: 'unknown'; reason: string };

export interface TasksListResponse {
  ok: boolean;
  now: number;
  desired: DesiredView;
  tasks: TaskRow[];
  legacy: LegacyTaskRow[];
}

export interface TaskLogLine {
  n: number;
  stream: 'out' | 'err' | 'meta';
  text: string;
}

export interface TaskRunLogResponse {
  ok: boolean;
  runId: string;
  state: TaskState;
  lines: TaskLogLine[];
  nextAfter: number | null;
  stdoutHidden: boolean;
}

export interface QqLoginResponse {
  ok: boolean;
  state: 'logged-in' | 'needs-login' | 'unknown';
  processes: { pid: number; start: number }[];
}

export interface BackupArchiveItem {
  name: string;
  time: number;
  size: number;
  result: 'verified' | 'unverified' | 'pending' | string;
}

export interface BackupSettings {
  dir: string | null;
  keepDays: number;
  time: string;
  includeState: boolean;
  resolvedDir: string;
}

export interface RestorePreview {
  runtimeRoot: string;
  archive: string;
  time: number;
  scope: 'everything' | 'memory' | string;
  contents: string[];
  replaces: string[];
  removes: string[];
  includesState: boolean;
  warnings: string[];
  confirmation: string;
}

export interface ReleasesStatusResponse {
  ok: boolean;
  settings: {
    policy: 'off' | 'notify' | 'auto';
    feedUrl: string;
  };
  current: string | null;
  available: {
    tag: string;
    assets: { name: string; url: string }[];
  } | null;
  lastCheckedAt: number | null;
  error: string | null;
  approval: {
    current: string;
    release: { tag: string };
    feedUrl: string;
    automatic: boolean;
  } | null;
  updateError: string | null;
}

export interface DependencyView {
  current: string | null;
  source: string | null;
  sourceUnavailable: string | null;
  latest: string | null;
  available: string | null;
  error: string | null;
}

export interface DependenciesResponse {
  ok: boolean;
  dependencies: Record<'snowluma' | 'dsh' | 'relay' | string, DependencyView>;
  approval: { name: string; current: string; version: string } | null;
  lastCheckedAt: number | null;
  updateError: string | null;
  result: {
    name?: string;
    from?: string;
    to?: string;
    status?: string;
    rollback?: string;
    reason?: string;
  } | null;
}

export function makeRequestId(prefix: string): string {
  const cleanPrefix = prefix.replace(/[^A-Za-z0-9_-]/g, '-');
  const rand = Math.random().toString(36).slice(2, 8);
  return `${cleanPrefix}-${Date.now()}-${rand}`.slice(0, 64);
}

export function fetchSchedule(): Promise<ScheduleListResponse> {
  return apiRequest<ScheduleListResponse>('/api/schedule', { method: 'GET' });
}

export function createScheduleReminder(payload: {
  conversation: string;
  text: string;
  at?: number;
  localAt?: string;
  repeat?: ScheduleRepeat;
}): Promise<{ ok: boolean; item: Record<string, unknown> }> {
  return apiRequest('/api/schedule', { method: 'POST', body: payload });
}

export function fetchScheduleHistory(
  limit = 100,
): Promise<{ ok: boolean; history: ScheduleHistoryRow[] }> {
  return apiRequest(`/api/schedule/history?limit=${limit}`, { method: 'GET' });
}

export function runScheduleAction(
  target: string,
  action: ScheduleActionName,
  body: Record<string, unknown> = {},
): Promise<Record<string, unknown>> {
  if (!/^[A-Za-z0-9:_-]+$/.test(target) || !target.includes(':')) {
    return Promise.reject(new Error(`非法的日程目标 ID: ${target}`));
  }
  return apiRequest(`/api/schedule/${target}/${action}`, {
    method: 'POST',
    body,
  });
}

export interface HealthResponse {
  ok: boolean;
  pid?: number;
  port?: number;
  administratorReady?: boolean;
  recoveryReason?: string;
  runtime?: unknown;
}

export function fetchHealth(): Promise<HealthResponse> {
  return apiRequest<HealthResponse>('/api/health', { method: 'GET' }).catch((err) => {
    if (err instanceof ApiRequestError && err.status === 200 && err.payload) {
      return {
        ok: false,
        pid: typeof err.payload.pid === 'number' ? err.payload.pid : undefined,
        port: typeof err.payload.port === 'number' ? err.payload.port : undefined,
        administratorReady:
          typeof err.payload.administratorReady === 'boolean'
            ? err.payload.administratorReady
            : false,
        recoveryReason:
          typeof err.payload.recoveryReason === 'string'
            ? err.payload.recoveryReason
            : err.message,
        runtime: err.payload.runtime,
      };
    }
    throw err;
  });
}

function buildQuery(params: Record<string, string | number | null | undefined>): string {
  const qs = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== '') {
      qs.set(key, String(value));
    }
  }
  const encoded = qs.toString();
  return encoded ? `?${encoded}` : '';
}

export function fetchTasks(): Promise<TasksListResponse> {
  return apiRequest<TasksListResponse>('/api/tasks', { method: 'GET' });
}

export function runTaskNow(
  task: string,
  requestId = makeRequestId(`ui-${task}`),
): Promise<{ ok: boolean; runId: string; state: TaskState; duplicate?: boolean; run: RunView }> {
  return apiRequest(`/api/tasks/${encodeURIComponent(task)}/run`, {
    method: 'POST',
    body: { requestId },
  });
}

export function fetchTaskRuns(params?: {
  task?: string;
  before?: number | null;
  limit?: number;
}): Promise<{ ok: boolean; runs: RunView[]; nextBefore: number | null }> {
  const suffix = buildQuery({
    task: params?.task,
    before: params?.before,
    limit: params?.limit,
  });
  return apiRequest(`/api/tasks/runs${suffix}`, { method: 'GET' });
}

export function fetchTaskRun(runId: string): Promise<{ ok: boolean; run: RunView }> {
  return apiRequest(`/api/tasks/runs/${encodeURIComponent(runId)}`, { method: 'GET' });
}

export function fetchTaskRunLog(
  runId: string,
  params?: { after?: number | null; limit?: number },
): Promise<TaskRunLogResponse> {
  const suffix = buildQuery({
    after: params?.after,
    limit: params?.limit,
  });
  return apiRequest<TaskRunLogResponse>(
    `/api/tasks/runs/${encodeURIComponent(runId)}/log${suffix}`,
    { method: 'GET' },
  );
}

export function setLifecycleState(payload: {
  state: 'running' | 'stopped' | 'maintenance';
  minutes?: number;
  reason?: string;
  confirm: true;
}): Promise<{
  ok: boolean;
  applied?: boolean;
  state?: string;
  record?: DesiredRecord;
}> {
  return apiRequest('/api/tasks/lifecycle', { method: 'POST', body: payload });
}

export function fetchQqLogin(): Promise<QqLoginResponse> {
  return apiRequest<QqLoginResponse>('/api/qq/login', { method: 'GET' });
}

export function queueRestart(
  requestId = makeRequestId('ui-restart'),
): Promise<{ ok: boolean; state: string; requestId: string; duplicate?: boolean }> {
  return apiRequest('/api/restart', {
    method: 'POST',
    body: { requestId, confirm: true },
  });
}

async function optionalPost<T>(path: string, body: unknown): Promise<OptionalResult<T>> {
  try {
    const data = await apiRequest<T>(path, { method: 'POST', body });
    return { available: true, data };
  } catch (err) {
    if (err instanceof ApiRequestError && (err.status === 404 || err.status === 503)) {
      return { available: false, status: err.status, reason: err.message };
    }
    throw err;
  }
}

export function optionalTestSend(payload: {
  kind: 'group' | 'private';
  id: number;
  message: string;
}): Promise<OptionalResult<{ ok: boolean; kind: string; id: number; message_id?: unknown }>> {
  return optionalPost('/api/test-send', payload);
}

export function optionalWorkspaceReset(): Promise<OptionalResult<{ ok: boolean }>> {
  return optionalPost('/api/workspace/reset', { confirm: true });
}

export function fetchBackups(): Promise<{
  ok: boolean;
  backups: BackupArchiveItem[];
  failedAttempts: RunView[];
}> {
  return apiRequest('/api/backups', { method: 'GET' });
}

export function fetchBackupSettings(): Promise<{ ok: boolean; settings: BackupSettings }> {
  return apiRequest('/api/backups/settings', { method: 'GET' });
}

export function saveBackupSettings(payload: {
  dir?: string | null;
  keepDays?: number;
  time?: string | null;
  includeState?: boolean;
}): Promise<{ ok: boolean; settings: BackupSettings }> {
  return apiRequest('/api/backups/settings', { method: 'POST', body: payload });
}

export function previewBackupRestore(
  archive: string,
  memoryOnly: boolean,
): Promise<{ ok: boolean; preview: RestorePreview }> {
  return apiRequest('/api/backups/restore/preview', {
    method: 'POST',
    body: { archive, memoryOnly },
  });
}

export function executeBackupRestore(
  archive: string,
  memoryOnly: boolean,
  confirmation: string,
): Promise<{ ok: boolean; queued?: boolean; pid?: number; message?: string }> {
  return apiRequest('/api/backups/restore', {
    method: 'POST',
    body: { archive, memoryOnly, confirmation },
  });
}

export function fetchReleases(): Promise<ReleasesStatusResponse> {
  return apiRequest<ReleasesStatusResponse>('/api/releases', { method: 'GET' });
}

export function saveReleaseSettings(payload: {
  policy?: 'off' | 'notify' | 'auto';
  feedUrl?: string;
}): Promise<{ ok: boolean; settings: ReleasesStatusResponse['settings'] }> {
  return apiRequest('/api/releases/settings', { method: 'POST', body: payload });
}

export function approveRelease(
  tag: string,
): Promise<{ ok: boolean; current: string; approved: string }> {
  return apiRequest('/api/releases/approve', {
    method: 'POST',
    body: { confirm: true, tag },
  });
}

export function fetchDependencies(): Promise<DependenciesResponse> {
  return apiRequest<DependenciesResponse>('/api/dependencies', { method: 'GET' });
}

export function checkDependencies(): Promise<DependenciesResponse> {
  return apiRequest<DependenciesResponse>('/api/dependencies/check', {
    method: 'POST',
    body: {},
  });
}

export function approveDependency(
  name: string,
  version: string,
): Promise<{ ok: boolean; name: string; current: string; approved: string }> {
  return apiRequest('/api/dependencies/approve', {
    method: 'POST',
    body: { confirm: true, name, version },
  });
}

// Cross-mount coordination for SchedulePage, SystemPage, BackupsTab, and UpdatesTab.
let settingsWriteGen = 0;
let activeScheduleWrites = 0;
let sharedScheduleSettings: SettingItemView[] | null = null;
let sharedScheduleStale = false;
const scheduleSyncListeners = new Set<() => void>();

function emitScheduleSync(): void {
  scheduleSyncListeners.forEach((fn) => fn());
}

export function getSettingsWriteGen(): number {
  return settingsWriteGen;
}

export function bumpSettingsWriteGen(): void {
  settingsWriteGen += 1;
}

export function hasActiveScheduleWrites(): boolean {
  return activeScheduleWrites > 0;
}

export function getSharedScheduleState(): {
  settings: SettingItemView[] | null;
  stale: boolean;
  activeWrites: number;
} {
  return {
    settings: sharedScheduleSettings,
    stale: sharedScheduleStale,
    activeWrites: activeScheduleWrites,
  };
}

export function beginScheduleWrite(): void {
  activeScheduleWrites += 1;
  settingsWriteGen += 1;
  emitScheduleSync();
}

export function finishScheduleWrite(): void {
  activeScheduleWrites = Math.max(0, activeScheduleWrites - 1);
  emitScheduleSync();
}

export function commitSharedSettingsRead(
  list: SettingItemView[],
  readStartGen: number,
): boolean {
  if (readStartGen !== settingsWriteGen) {
    return false;
  }
  sharedScheduleSettings = list;
  sharedScheduleStale = false;
  emitScheduleSync();
  return true;
}

export function markSharedSettingsReadFailed(readStartGen: number): void {
  if (readStartGen !== settingsWriteGen) {
    return;
  }
  sharedScheduleStale = true;
  emitScheduleSync();
}

export function markSharedScheduleStale(): void {
  sharedScheduleStale = true;
  emitScheduleSync();
}

export async function refreshSharedScheduleSettings(): Promise<SettingItemView[] | null> {
  const readStartGen = settingsWriteGen;
  try {
    const res = await fetchSettingsList();
    const list = Array.isArray(res.settings) ? res.settings : [];
    if (!commitSharedSettingsRead(list, readStartGen)) {
      return null;
    }
    return list;
  } catch {
    markSharedSettingsReadFailed(readStartGen);
    return null;
  }
}

export function subscribeScheduleSync(listener: () => void): () => void {
  scheduleSyncListeners.add(listener);
  return () => {
    scheduleSyncListeners.delete(listener);
  };
}

let nextSystemMountSeq = 1;
let backupDirVersion = 0;
let activeBackupSaves = 0;
let activeBackupDirSaves = 0;
let activeExternalBackupTimeSaves = 0;
let backupSaveVersion = 0;
let activeReleaseSaves = 0;
let releaseSaveVersion = 0;

interface SystemSyncHandlers {
  onBackupDirStarted: () => void;
  onBackupSaveSettled: () => void;
  onReleaseSaveSettled: () => void;
  onActiveSavesChanged: () => void;
}

const systemSyncListeners = new Map<number, SystemSyncHandlers>();

export function nextSystemMountId(): number {
  return nextSystemMountSeq++;
}

export function getBackupDirVersion(): number {
  return backupDirVersion;
}

export function getBackupSaveVersion(): number {
  return backupSaveVersion;
}

export function getReleaseSaveVersion(): number {
  return releaseSaveVersion;
}

export function hasActiveBackupSaves(): boolean {
  return activeBackupSaves > 0 || activeExternalBackupTimeSaves > 0;
}

export function hasPendingBackupDirSaves(): boolean {
  return activeBackupDirSaves > 0;
}

export function hasActiveReleaseSaves(): boolean {
  return activeReleaseSaves > 0;
}

export function beginExternalBackupTimeSave(): void {
  activeExternalBackupTimeSaves += 1;
  systemSyncListeners.forEach((h) => {
    h.onActiveSavesChanged();
  });
}

export function finishExternalBackupTimeSave(): void {
  activeExternalBackupTimeSaves = Math.max(0, activeExternalBackupTimeSaves - 1);
  backupSaveVersion += 1;
  systemSyncListeners.forEach((h) => {
    h.onActiveSavesChanged();
    h.onBackupSaveSettled();
  });
}

export function beginBackupSave(hasDirChange: boolean): void {
  activeBackupSaves += 1;
  if (hasDirChange) {
    activeBackupDirSaves += 1;
    backupDirVersion += 1;
  }
  systemSyncListeners.forEach((h) => {
    h.onActiveSavesChanged();
    if (hasDirChange) h.onBackupDirStarted();
  });
}

export function finishBackupSave(hasDirChange: boolean, originMountId: number): void {
  activeBackupSaves = Math.max(0, activeBackupSaves - 1);
  if (hasDirChange) {
    activeBackupDirSaves = Math.max(0, activeBackupDirSaves - 1);
    backupDirVersion += 1;
  }
  backupSaveVersion += 1;
  systemSyncListeners.forEach((h, mountId) => {
    h.onActiveSavesChanged();
    if (mountId !== originMountId) {
      h.onBackupSaveSettled();
    }
  });
}

export function beginReleaseSave(): void {
  activeReleaseSaves += 1;
  systemSyncListeners.forEach((h) => {
    h.onActiveSavesChanged();
  });
}

export function finishReleaseSave(originMountId: number): void {
  activeReleaseSaves = Math.max(0, activeReleaseSaves - 1);
  releaseSaveVersion += 1;
  systemSyncListeners.forEach((h, mountId) => {
    h.onActiveSavesChanged();
    if (mountId !== originMountId) {
      h.onReleaseSaveSettled();
    }
  });
}

export function subscribeSystemSync(
  mountId: number,
  handlers: SystemSyncHandlers,
): () => void {
  systemSyncListeners.set(mountId, handlers);
  return () => {
    systemSyncListeners.delete(mountId);
  };
}
