export interface AuthStatusResponse {
  ok: boolean;
  passwordSet: boolean;
  authenticated: boolean;
  authKind: 'browser' | 'desktop' | 'machine' | null;
  canonicalOrigin?: string;
  tokenFile?: string;
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
  redacted?: boolean;
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
  return typeof window !== 'undefined' && window.location.hostname === '127.0.0.1';
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

function readDesktopBearer(): string {
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

export async function apiRequest<T = Record<string, unknown>>(
  path: string,
  options: {
    method?: 'GET' | 'POST';
    body?: unknown;
    skipUnauthorizedRedirect?: boolean;
  } = {},
): Promise<T> {
  const method = options.method ?? (options.body !== undefined ? 'POST' : 'GET');
  const headers: Record<string, string> = {};
  const desktopBearer = readDesktopBearer();
  if (desktopBearer) {
    headers['x-console-token'] = desktopBearer;
  }
  let serialized: string | undefined;
  if (options.body !== undefined) {
    headers['content-type'] = 'application/json';
    serialized = JSON.stringify(options.body);
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
    try {
      parsed = JSON.parse(text) as Record<string, unknown>;
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

export async function fetchPendingItems(): Promise<{ items?: unknown[] }> {
  return apiRequest<{ items?: unknown[] }>('/api/pending', { method: 'GET' });
}

export async function fetchSettingsList(): Promise<SettingsResponse> {
  return apiRequest<SettingsResponse>('/api/settings', { method: 'GET' });
}

export async function saveSettingValue(
  setting: string,
  value: unknown,
  confirm = false,
): Promise<Record<string, unknown>> {
  return apiRequest('/api/settings/set', {
    method: 'POST',
    body: confirm ? { setting, value, confirm: true } : { setting, value },
  });
}

export async function resetSettingValue(
  setting: string,
  confirm = false,
): Promise<Record<string, unknown>> {
  return apiRequest('/api/settings/reset', {
    method: 'POST',
    body: confirm ? { setting, confirm: true } : { setting },
  });
}

export async function fetchConversationStates(): Promise<Record<string, unknown>> {
  return apiRequest<Record<string, unknown>>('/api/socialV2/states', { method: 'GET' });
}

export async function fetchQqLoginState(): Promise<{ state?: string }> {
  return apiRequest<{ state?: string }>('/api/qq/login', { method: 'GET' });
}
