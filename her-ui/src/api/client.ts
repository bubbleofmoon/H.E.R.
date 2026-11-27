export interface AuthStatusResponse {
  ok: boolean;
  passwordSet: boolean;
  authenticated: boolean;
  authKind: 'browser' | 'desktop' | 'machine' | null;
  canonicalOrigin?: string;
  tokenFile?: string;
  requirePasswordLocally?: boolean;
  consecutiveFailures?: number;
  retryAfterMs?: number;
  retryAfterSeconds?: number;
  error?: string;
}

export interface ApiErrorPayload {
  ok: false;
  error?: string;
  passwordSet?: boolean;
  consecutiveFailures?: number;
  retryAfterMs?: number;
  retryAfterSeconds?: number;
}

export class ApiRequestError extends Error {
  status: number;
  payload: Record<string, unknown>;

  constructor(status: number, message: string, payload: Record<string, unknown> = {}) {
    super(message);
    this.status = status;
    this.payload = payload;
  }
}

export interface SettingItemView {
  key: string;
  label: string;
  hint: string;
  group: string;
  applies: string;
  confirm?: boolean;
  managedBy?: string;
  isSet?: boolean;
  boundElsewhere?: boolean;
  source?: 'settings' | 'file' | null;
  fallback?: { source?: string; value?: unknown };
  redacted?: boolean;
  overriddenBy?: { source?: string; value?: unknown };
  value?: unknown;
  default?: unknown;
  stored?: unknown;
  kind: {
    type: string;
    min?: number;
    max?: number;
    maxChars?: number;
    options?: string[];
  };
}

export interface SettingsResponse {
  ok: boolean;
  settings: SettingItemView[];
  storeError?: string;
}

export interface ConversationStateSummary {
  key: string;
  name?: string;
  phase?: string;
  unread?: number;
  nextWakeAt?: number;
}

type UnauthorizedHandler = () => void;

let onUnauthorized: UnauthorizedHandler | null = null;

export function setUnauthorizedHandler(handler: UnauthorizedHandler | null): void {
  onUnauthorized = handler;
}

export function isDesktopOrigin(): boolean {
  return (
    typeof window !== 'undefined' &&
    (window.location.hostname === '127.0.0.1' || window.location.hostname === 'localhost')
  );
}

export function captureDesktopTokenFromUrl(): void {
  if (typeof window === 'undefined' || !isDesktopOrigin()) {
    return;
  }
  const url = new URL(window.location.href);
  const queryToken = url.searchParams.get('token');
  if (queryToken) {
    try {
      window.sessionStorage.setItem('consoleToken', queryToken);
      window.localStorage.setItem('consoleToken', queryToken);
    } catch {
      // Storage may be restricted; keep best-effort behavior.
    }
    url.searchParams.delete('token');
    const clean = `${url.pathname}${url.search}${url.hash}`;
    window.history.replaceState(null, '', clean || '/');
  }
}

export function readDesktopBearer(): string {
  if (typeof window === 'undefined' || !isDesktopOrigin()) {
    return '';
  }
  try {
    return (
      window.sessionStorage.getItem('consoleToken') ||
      window.localStorage.getItem('consoleToken') ||
      ''
    );
  } catch {
    return '';
  }
}

export function clearDesktopBearer(): void {
  if (typeof window === 'undefined' || !isDesktopOrigin()) {
    return;
  }
  try {
    window.sessionStorage.removeItem('consoleToken');
    window.localStorage.removeItem('consoleToken');
  } catch {
    // Storage may be restricted; keep best-effort behavior.
  }
}

export async function apiRequest<T = Record<string, unknown>>(
  path: string,
  options: {
    method?: 'GET' | 'POST' | 'PATCH' | 'DELETE';
    body?: unknown;
    skipUnauthorizedRedirect?: boolean;
    quoteNonIntegerNumbers?: boolean;
  } = {},
): Promise<T> {
  const method = options.method ?? (options.body !== undefined ? 'POST' : 'GET');
  const headers: Record<string, string> = {};
  const desktopBearer = readDesktopBearer();
  if (desktopBearer) {
    headers['x-console-token'] = desktopBearer;
  }
  let serialized: string | undefined;
  if (method !== 'GET') {
    headers['content-type'] = 'application/json';
    if (options.body !== undefined) {
      serialized = JSON.stringify(options.body);
    }
  }

  const response = await fetch(path, {
    method,
    headers,
    credentials: 'same-origin',
    body: serialized,
  });

  const text = await response.text();
  let parsed: Record<string, unknown> = {};
  if (text) {
    const jsonSource = options.quoteNonIntegerNumbers
      ? text.replace(
          /"(?:\\.|[^"\\])*"|-?\d+(?:\.\d+(?:[eE][+-]?\d+)?|[eE][+-]?\d+)/g,
          (match) => (match.startsWith('"') ? match : JSON.stringify(match)),
        )
      : text;
    try {
      parsed = JSON.parse(jsonSource) as Record<string, unknown>;
    } catch {
      parsed = { ok: false, error: text };
    }
  }

  if (response.status === 401 && !options.skipUnauthorizedRedirect && onUnauthorized) {
    const currentPath = typeof window !== 'undefined' ? window.location.pathname : '/';
    if (currentPath !== '/login' && currentPath !== '/setup') {
      onUnauthorized();
    }
  }

  if (!response.ok || parsed.ok === false) {
    const message =
      typeof parsed.error === 'string' && parsed.error
        ? parsed.error
        : `请求失败 (${response.status})`;
    throw new ApiRequestError(response.status, message, parsed);
  }

  return parsed as T;
}

export async function fetchAuthStatus(): Promise<AuthStatusResponse> {
  return apiRequest<AuthStatusResponse>('/api/auth/status', {
    method: 'GET',
    skipUnauthorizedRedirect: true,
  });
}

export async function submitFirstRunSetup(consoleToken: string, password: string): Promise<void> {
  await apiRequest('/api/auth/first-run', {
    method: 'POST',
    body: { consoleToken, password },
    skipUnauthorizedRedirect: true,
  });
}

export async function submitPasswordLogin(password: string): Promise<void> {
  await apiRequest('/api/auth/login', {
    method: 'POST',
    body: { password },
    skipUnauthorizedRedirect: true,
  });
}

export async function submitLogout(): Promise<void> {
  await apiRequest('/api/auth/logout', {
    method: 'POST',
    body: {},
    skipUnauthorizedRedirect: true,
  });
}

export async function fetchConsoleStatus(): Promise<Record<string, unknown>> {
  return apiRequest<Record<string, unknown>>('/api/status', { method: 'GET' });
}

export async function fetchPendingItems(): Promise<{ pending?: unknown[]; items?: unknown[] }> {
  return apiRequest<{ pending?: unknown[]; items?: unknown[] }>('/api/pending', { method: 'GET' });
}

export async function fetchReleaseStatus(): Promise<{ available?: { tag?: string } | null }> {
  return apiRequest<{ available?: { tag?: string } | null }>('/api/releases', { method: 'GET' });
}

export async function fetchSettingsList(): Promise<SettingsResponse> {
  return apiRequest<SettingsResponse>('/api/settings', { method: 'GET' });
}

const settingWriteChains = new Map<string, Promise<unknown>>();
const activeSettingWrites = new Map<string, number>();

export function isSettingWriteLocked(key: string): boolean {
  return (activeSettingWrites.get(key) ?? 0) > 0;
}

async function runWithSettingWriteLock<T>(key: string, fn: () => Promise<T>): Promise<T> {
  activeSettingWrites.set(key, (activeSettingWrites.get(key) ?? 0) + 1);
  const prev = settingWriteChains.get(key) ?? Promise.resolve();
  const next = prev.catch(() => undefined).then(fn);
  const tracked = next.catch(() => undefined);
  settingWriteChains.set(key, tracked);
  try {
    return await next;
  } finally {
    const remaining = (activeSettingWrites.get(key) ?? 1) - 1;
    if (remaining <= 0) {
      activeSettingWrites.delete(key);
    } else {
      activeSettingWrites.set(key, remaining);
    }
    if (settingWriteChains.get(key) === tracked) {
      settingWriteChains.delete(key);
    }
  }
}

export async function saveSettingValue(
  setting: string,
  value: unknown,
  confirm = false,
  expectedEndpoint?: string,
): Promise<Record<string, unknown>> {
  const body: Record<string, unknown> = confirm
    ? { setting, value, confirm: true }
    : { setting, value };
  if (expectedEndpoint !== undefined) {
    body.expectedEndpoint = expectedEndpoint;
  }
  return runWithSettingWriteLock(setting, () =>
    apiRequest('/api/settings/set', {
      method: 'POST',
      body,
    }),
  );
}

export async function resetSettingValue(
  setting: string,
  confirm = false,
): Promise<Record<string, unknown>> {
  return runWithSettingWriteLock(setting, () =>
    apiRequest('/api/settings/reset', {
      method: 'POST',
      body: confirm ? { setting, confirm: true } : { setting },
    }),
  );
}

export async function fetchConversationStates(): Promise<Record<string, unknown>> {
  return apiRequest<Record<string, unknown>>('/api/socialV2/states', { method: 'GET' });
}

export async function fetchQqLoginState(): Promise<{ state?: string }> {
  return apiRequest<{ state?: string }>('/api/qq/login', { method: 'GET' });
}
