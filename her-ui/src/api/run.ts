import { ApiRequestError, apiRequest } from './client';

export interface SnowlumaHealth {
  connected: boolean;
  accountOnline: boolean | null;
  good: boolean | null;
  goodSource: string | null;
  goodError: string | null;
  lastPacketAgoMs: number | null;
  lastPacketKind: string | null;
  silent: boolean;
  degraded: boolean;
  hint: string | null;
}

export interface StatusDetail {
  mode: string;
  closedAgentPreset?: string;
  role: string | null;
  roleMode: 'silent' | 'active';
  ownerQQ?: number;
  allowGroups?: string[];
  allowPrivate?: string[];
  socialV2Paused: boolean;
  activity: string;
  dshReady: boolean;
  snowluma?: SnowlumaHealth;
}

export interface PresetOption {
  id: string;
  trust: string;
}

export interface PendingQuestionOption {
  label: string;
}

export interface PendingQuestionItem {
  id?: string;
  question: string;
  options?: PendingQuestionOption[];
}

export interface PendingItem {
  key: string;
  kind: 'question' | 'approval';
  sessionId?: string;
  toolName?: string | null;
  target?: string;
  message?: string;
  requestId?: string;
  status?: string;
  questions?: PendingQuestionItem[];
}

export interface TokenSeriesResponse {
  ok: boolean;
  currency: string;
  symbol: string;
  totals: {
    cost: number;
    tokens: number;
    turns: number;
    steps: number;
  };
}

export interface TokenSummaryResponse {
  ok: boolean;
  currency: string;
  symbol: string;
  totals: {
    cost: number;
    turns: number;
    steps: number;
    conversations: number;
    totalTokens: number;
    cacheHitRate: number;
    avgCostPerTurn: number;
  };
}

export interface SessionEntry {
  key: string;
  sessionId: string;
  owner: boolean;
}

export interface LiveRestartInfo {
  state: 'none' | 'required' | 'unknown';
  reason: string;
  serving?: string[];
}

export interface PersistedLiveRestart {
  info: LiveRestartInfo;
  watchdogRunId?: string | null;
}

export const LIVE_RESTART_STORAGE_KEY = 'herLiveRestartRequired';

export function readPersistedLiveRestart(): PersistedLiveRestart | null {
  if (typeof window === 'undefined') return null;
  try {
    const saved = window.sessionStorage.getItem(LIVE_RESTART_STORAGE_KEY);
    if (!saved) return null;
    const parsed = JSON.parse(saved) as PersistedLiveRestart | LiveRestartInfo;
    if ('info' in parsed && parsed.info) {
      return parsed;
    }
    if ('state' in parsed) {
      return { info: parsed };
    }
    return null;
  } catch {
    return null;
  }
}

export function persistLiveRestartBanner(
  info: LiveRestartInfo | null,
  watchdogRunId?: string | null,
): PersistedLiveRestart | null {
  const next: PersistedLiveRestart | null =
    info && info.state === 'required' ? { info, watchdogRunId: watchdogRunId ?? null } : null;
  if (typeof window !== 'undefined') {
    try {
      if (next) {
        window.sessionStorage.setItem(LIVE_RESTART_STORAGE_KEY, JSON.stringify(next));
      } else {
        window.sessionStorage.removeItem(LIVE_RESTART_STORAGE_KEY);
      }
    } catch {
      // Ignore storage failures cleanly.
    }
  }
  return next;
}

export interface ScopeChangeResponse {
  ok: boolean;
  conversation: string;
  action: 'admit' | 'remove';
  changed: string[];
  revoked: string[];
  liveRestart: LiveRestartInfo;
  scope: {
    notAllowed: string[];
    notServed: string[];
  };
  mirrorError?: string;
}

export interface WakeConfigSnapshot {
  mode?: string;
  infinite?: boolean;
  sleepUntil?: string | null;
  confirmedAt?: number;
  noActionCount?: number;
  triggers?: Record<string, unknown>;
}

export interface ConversationSummaryItem {
  key: string;
  wakeConfig: WakeConfigSnapshot;
  unreadCount: number;
  recentCount: number;
  lastWakeReason: string | null;
  lastAiReplyAt: number;
  noActionCount: number;
}

export interface ConversationStateDetail {
  ok: boolean;
  key: string;
  wakeConfig: WakeConfigSnapshot;
  wakeSafety: {
    hard: boolean;
    timed: boolean;
    soft: boolean;
    guaranteed: boolean;
    stale: boolean;
  };
  unreadCount: number;
  recentCount: number;
  lastWakeReason: string | null;
  lastAiReplyAt: number;
}

export interface LightMemorySnapshot {
  ok: boolean;
  key: string;
  category: string;
  formatted: string;
  raw: {
    activeTopics: {
      text: string;
      lastMentionAt?: number;
      participants?: string[];
      pendingQuestion?: string;
    }[];
    pendingThoughts: {
      text: string;
      createdAt?: number;
      expiresAt?: number;
      motivation?: string;
    }[];
    memberImpressions: Record<
      string,
      {
        traits?: string[];
        interactionCount?: number;
        lastSeenAt?: number;
      }
    >;
  };
}

export interface SendOutboxBatch {
  batchId: string;
  key: string;
  createdAt: number;
  total: number;
  sent: number;
  unknown: number[];
  pending: number[];
  items: {
    idx: number;
    state: string;
    text: string;
    messageId?: number | null;
    verified?: boolean;
  }[];
}

export interface TurnRecord {
  turnId: string;
  code?: string;
  key: string;
  backend?: string;
  reason?: string;
  state: string;
  sessionId?: string;
  beganAt?: number;
  endedAt?: number | null;
  pauseReason?: string | null;
  pauseDetail?: string | null;
  pausedAt?: number | null;
  resolution?: string | null;
  sends?: {
    batchId: string;
    total: number;
    sent: number;
    unknown: number[];
    pending: number[];
  }[];
}

export interface PausedTurnSummary {
  turnId: string;
  code?: string;
  key: string;
  reason: string;
  detail?: string | null;
}

export interface TurnsResponse {
  turns: TurnRecord[];
  paused: PausedTurnSummary[];
  pausedCount: number;
}

export interface InboundGap {
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
  unresolvedGaps: InboundGap[];
  gaps: InboundGap[];
}

export interface PersonaListResponse {
  ok: boolean;
  default: string | null;
  overrides: Record<string, string>;
  personas: {
    id: string;
    name: string;
    activeVersion: number | null;
    latestVersion: number | null;
    pendingVersions: number[];
  }[];
}

export interface ConversationSettingsListResponse {
  ok: boolean;
  note: string;
  storeError?: string;
  settings: {
    key: string;
    scope: string;
    group: string;
    label: string;
    hint: string;
    applies: string;
    default: unknown;
    global: string;
  }[];
  conversations: {
    conversation: string;
    admitted: boolean;
    engineToday: string;
    settings: Record<string, { stored: unknown; value: unknown }>;
  }[];
}

export interface ModelRoutesResponse {
  ok: boolean;
  setting: string;
  firstRoute: string | null;
  error?: string;
  routes: {
    id: string;
    model: string;
    reasoningEffort: string | null;
    retry429: boolean;
    health: { state: 'ok' | 'failed' | 'unknown'; since?: number; class?: string };
  }[];
  allFailed: boolean;
  healthError?: string;
  storeError?: string;
  conversations: {
    conversation: string;
    admitted: boolean;
    stored: string | null;
    route: string | null;
    inChain: boolean;
    order: string[];
  }[];
}

export async function fetchStatusDetail(): Promise<StatusDetail> {
  return apiRequest<StatusDetail>('/api/status', { method: 'GET' });
}

export async function fetchPresetsOptional(): Promise<{
  available: boolean;
  presets: PresetOption[];
}> {
  try {
    const res = await apiRequest<{ presets?: PresetOption[] }>('/api/presets', { method: 'GET' });
    return { available: true, presets: Array.isArray(res.presets) ? res.presets : [] };
  } catch (err) {
    if (err instanceof ApiRequestError && (err.status === 500 || err.status === 503)) {
      return { available: false, presets: [] };
    }
    throw err;
  }
}

export async function saveMode(
  mode: string,
  closedAgentPreset?: string,
): Promise<{ ok: boolean; mode: string; closedAgentPreset?: string; dshSynced?: boolean }> {
  const body =
    mode === 'closed-agent'
      ? { mode, closedAgentPreset: closedAgentPreset ?? '' }
      : closedAgentPreset !== undefined
        ? { mode, closedAgentPreset }
        : { mode };
  return apiRequest('/api/mode', {
    method: 'POST',
    body,
  });
}

export async function saveRoleMode(
  mode: 'silent' | 'active',
): Promise<{ ok: boolean; roleMode: 'silent' | 'active' }> {
  return apiRequest('/api/role-mode', {
    method: 'POST',
    body: { mode },
  });
}

export async function saveSocialV2Activity(
  paused: boolean,
): Promise<{ ok: boolean; paused: boolean }> {
  return apiRequest('/api/socialV2/activity', {
    method: 'POST',
    body: { paused },
  });
}

export async function fetchPendingDetailed(): Promise<{ pending: PendingItem[] }> {
  const res = await apiRequest<{ pending?: PendingItem[] }>('/api/pending', { method: 'GET' });
  return { pending: Array.isArray(res.pending) ? res.pending : [] };
}

export async function fetchTokenSeries24h(): Promise<TokenSeriesResponse> {
  return apiRequest<TokenSeriesResponse>('/api/tokens/series?hours=24&buckets=24', {
    method: 'GET',
  });
}

export async function fetchTokenSummary(): Promise<TokenSummaryResponse> {
  return apiRequest<TokenSummaryResponse>('/api/tokens/summary?limit=50', { method: 'GET' });
}

export async function fetchAllStates(): Promise<{ conversations: ConversationSummaryItem[] }> {
  const res = await apiRequest<{ conversations?: ConversationSummaryItem[] }>(
    '/api/socialV2/states',
    { method: 'GET' },
  );
  return { conversations: Array.isArray(res.conversations) ? res.conversations : [] };
}

export async function fetchSessions(): Promise<{
  sessions: SessionEntry[];
  rotationFailures: Record<string, unknown>;
}> {
  const res = await apiRequest<{
    sessions?: SessionEntry[];
    rotationFailures?: Record<string, unknown>;
  }>('/api/sessions', { method: 'GET' });
  return {
    sessions: Array.isArray(res.sessions) ? res.sessions : [],
    rotationFailures: res.rotationFailures ?? {},
  };
}

export async function resetSession(key: string): Promise<{ ok: boolean; archived: string }> {
  return apiRequest('/api/session/reset', {
    method: 'POST',
    body: { key },
  });
}

export async function changeConversationScope(
  conversation: string,
  action: 'admit' | 'remove',
  confirm: boolean,
): Promise<ScopeChangeResponse> {
  return apiRequest<ScopeChangeResponse>('/api/conversations/scope', {
    method: 'POST',
    body: { conversation, action, confirm },
  });
}

export async function triggerWatchdogTask(): Promise<{
  ok: boolean;
  runId?: string;
  state?: string;
  duplicate?: boolean;
}> {
  return apiRequest('/api/tasks/watchdog/run', {
    method: 'POST',
    body: { requestId: `ui-watchdog-${Date.now()}` },
  });
}

export async function fetchConversationState(key: string): Promise<ConversationStateDetail> {
  return apiRequest<ConversationStateDetail>(
    `/api/socialV2/state?key=${encodeURIComponent(key)}`,
    { method: 'GET' },
  );
}

export async function wakeConversation(
  key: string,
  reason = 'admin',
): Promise<{ ok: boolean; reason: string; firstWake: boolean }> {
  return apiRequest('/api/socialV2/wake', {
    method: 'POST',
    body: { key, reason },
  });
}

export async function fetchInitialPrompt(key: string): Promise<{
  ok: boolean;
  exists: boolean;
  text: string;
  maxChars: number;
  writable: boolean;
}> {
  return apiRequest(`/api/socialV2/initial-prompt?key=${encodeURIComponent(key)}`, {
    method: 'GET',
  });
}

export async function saveInitialPrompt(key: string, text: string): Promise<{ ok: boolean }> {
  return apiRequest('/api/socialV2/initial-prompt', {
    method: 'POST',
    body: { key, text },
  });
}

export async function fetchLightMemory(key: string): Promise<LightMemorySnapshot> {
  return apiRequest<LightMemorySnapshot>(
    `/api/socialV2/memory?key=${encodeURIComponent(key)}`,
    { method: 'GET' },
  );
}

export async function appendLightMemory(payload: {
  key: string;
  category: 'activeTopic' | 'pendingThought' | 'memberImpression';
  content: string;
  extra?: Record<string, unknown>;
}): Promise<{ ok: boolean }> {
  return apiRequest('/api/socialV2/memory-append', {
    method: 'POST',
    body: payload,
  });
}

export async function updateLightMemory(payload: {
  key: string;
  category: 'activeTopic' | 'pendingThought' | 'memberImpression';
  oldContent?: string;
  target?: string;
  newContent?: string | null;
  newExtra?: Record<string, unknown>;
}): Promise<{ ok: boolean }> {
  return apiRequest('/api/socialV2/memory-update', {
    method: 'POST',
    body: payload,
  });
}

export async function removeLightMemory(payload: {
  key: string;
  category: 'activeTopic' | 'pendingThought' | 'memberImpression';
  content?: string;
  target?: string;
}): Promise<{ ok: boolean }> {
  return apiRequest('/api/socialV2/memory-remove', {
    method: 'POST',
    body: payload,
  });
}

export async function clearLightMemory(
  key: string,
  category?: 'activeTopic' | 'pendingThought' | 'memberImpression',
): Promise<{ ok: boolean }> {
  return apiRequest('/api/socialV2/memory-clear', {
    method: 'POST',
    body: category ? { key, category } : { key },
  });
}

export async function fetchSendOutbox(
  key: string,
): Promise<{ ok: boolean; batches: SendOutboxBatch[] }> {
  return apiRequest(`/api/socialV2/send-outbox?key=${encodeURIComponent(key)}`, {
    method: 'GET',
  });
}

export async function fetchTurns(): Promise<TurnsResponse> {
  return apiRequest<TurnsResponse>('/api/turns', { method: 'GET' });
}

export async function resolveTurn(
  turnId: string,
  action: 'abandon' | 'retry',
  confirm = false,
): Promise<{ ok: boolean }> {
  return apiRequest('/api/turns/resolve', {
    method: 'POST',
    body: confirm ? { turnId, action, confirm: true } : { turnId, action },
  });
}

export async function fetchInboundGaps(): Promise<InboundGapsResponse> {
  return apiRequest<InboundGapsResponse>('/api/inbound/gaps', { method: 'GET' });
}

export async function retryInboundGaps(conversation: string): Promise<{ ok: boolean; reset: number }> {
  return apiRequest('/api/inbound/gaps/retry', {
    method: 'POST',
    body: { conversation },
  });
}

export async function fetchPersonasList(): Promise<PersonaListResponse> {
  return apiRequest<PersonaListResponse>('/api/personas', { method: 'GET' });
}

export async function assignConversationPersona(
  conversation: string,
  persona: string | null,
): Promise<{
  ok: boolean;
  rebriefed?: string[];
  rebriefFailed?: { conversation?: string; error?: string }[];
}> {
  return apiRequest('/api/personas/assign', {
    method: 'POST',
    body: { conversation, persona },
  });
}

export interface SettingsHistoryEntry {
  entry: number | string;
  atMs: number;
  setting: string;
  conversation?: string | null;
  old: unknown;
  new: unknown;
  source?: string;
  actor?: string;
}

export async function fetchConversationSettingsHistory(
  conversation: string,
): Promise<{ ok: boolean; entries: SettingsHistoryEntry[] }> {
  return apiRequest(
    `/api/settings/history?conversation=${encodeURIComponent(conversation)}`,
    { method: 'GET' },
  );
}

export async function undoSetting(
  entry: number | string,
): Promise<{ ok: boolean; mirrorError?: string }> {
  return apiRequest('/api/settings/undo', {
    method: 'POST',
    body: { entry },
  });
}

export async function fetchConversationSettings(): Promise<ConversationSettingsListResponse> {
  return apiRequest<ConversationSettingsListResponse>('/api/settings/conversations', {
    method: 'GET',
  });
}

export async function fetchModelRoutes(): Promise<ModelRoutesResponse> {
  return apiRequest<ModelRoutesResponse>('/api/settings/models', { method: 'GET' });
}

export async function setConversationSetting(
  conversation: string,
  setting: 'conversation.engine' | 'conversation.route',
  value: unknown,
): Promise<{ ok: boolean; mirrorError?: string }> {
  return apiRequest('/api/settings/conversation/set', {
    method: 'POST',
    body: { conversation, setting, value },
  });
}

export async function resetConversationSetting(
  conversation: string,
  setting: 'conversation.engine' | 'conversation.route',
): Promise<{ ok: boolean; mirrorError?: string }> {
  return apiRequest('/api/settings/conversation/reset', {
    method: 'POST',
    body: { conversation, setting },
  });
}

export async function switchConversationEngine(
  conversation: string,
  engine: 'dsh' | 'harness',
): Promise<{ ok: boolean; mirrorError?: string }> {
  return apiRequest('/api/settings/conversation/switch', {
    method: 'POST',
    body: { conversation, engine },
  });
}
