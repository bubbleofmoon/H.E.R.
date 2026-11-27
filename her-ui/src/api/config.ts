import { apiRequest } from './client';
import {
  fetchPresetsOptional,
  fetchSendOutbox,
  fetchStatusDetail,
  PresetOption,
  saveMode,
  saveRoleMode,
  saveSocialV2Activity as setSocialV2Activity,
  SendOutboxBatch,
  StatusDetail,
} from './run';

export {
  fetchPresetsOptional,
  fetchSendOutbox,
  fetchStatusDetail,
  saveMode,
  saveRoleMode,
  setSocialV2Activity,
};
export type { PresetOption, SendOutboxBatch, StatusDetail };

export interface SocialV2FeedbackEntry {
  id: string;
  key?: string;
  level: 'info' | 'warning' | 'error' | string;
  message: string;
  time?: string | number;
}

export interface ToolLogEntry {
  time?: number;
  at?: number;
  key?: string;
  conversation?: string;
  tool?: string;
  name?: string;
  ok?: boolean;
  status?: string;
  summary?: string;
  detail?: unknown;
  [k: string]: unknown;
}

export interface RoutePriceInfo {
  label?: string;
  currency?: string;
  symbol?: string;
  unit?: number | string;
  tier?: string;
  estimated?: boolean;
  cacheHit?: number;
  cacheMiss?: number;
  cacheWrite?: number;
  output?: number;
}

export interface RateInfo {
  model?: string;
  label?: string;
  tier?: string;
  tierLabel?: string;
  currency?: string;
  symbol?: string;
  unit?: number;
  estimated?: boolean;
  cacheHit?: number;
  cacheMiss?: number;
  cacheWrite?: number;
  output?: number;
}

export interface DetailedModelRoute {
  id: string;
  model: string;
  reasoningEffort?: string | null;
  maxOutputTokens?: number | null;
  retry429?: boolean;
  health?: {
    state: 'ok' | 'failed' | 'unknown' | string;
    since?: number | null;
    class?: string | null;
  };
  price?: RoutePriceInfo | null;
}

export interface ConversationRouteBinding {
  conversation: string;
  admitted?: boolean;
  stored?: string | null;
  route?: string | null;
  inChain?: boolean;
  order?: string[];
}

export interface DetailedModelsResponse {
  ok?: boolean;
  setting?: string;
  firstRoute?: string | null;
  effectiveModelEndpoint?: string | null;
  effectiveModelEndpointSource?: string | null;
  error?: string | null;
  routes: DetailedModelRoute[];
  allFailed?: boolean;
  healthError?: string | null;
  storeError?: string | null;
  conversations: ConversationRouteBinding[];
}

export interface LegacyModelChainResponse {
  ok?: boolean;
  key?: string | null;
  chain: {
    provider?: string;
    model?: string;
    effort?: string | null;
    source?: string;
  }[];
  global?: {
    provider?: string;
    model?: string;
    reasoningEffort?: string | null;
  };
}

export interface TokenSeriesBucket {
  t: number;
  cost: number;
  tokens: number;
  cacheMiss?: number;
  cacheHit?: number;
  cacheWrite?: number;
  output?: number;
  turns?: number;
  steps?: number;
  tier?: string;
  keys?: Record<string, unknown>;
}

export interface TokenSeriesResponse {
  ok?: boolean;
  generatedAt?: number;
  currency?: string;
  symbol?: string;
  hours: number;
  bucketMs?: number;
  from?: number;
  to?: number;
  buckets: TokenSeriesBucket[];
  totals: {
    cost: number;
    tokens: number;
    turns: number;
    steps: number;
  };
  busiest?: TokenSeriesBucket | null;
  estimated?: boolean;
  countedSamples?: number;
  offChartCost?: number;
  offChartTokens?: number;
}

export interface TokenBucketsBreakdown {
  cacheMiss?: number;
  cacheHit?: number;
  cacheWrite?: number;
  output?: number;
  reasoning?: number;
  total?: number;
  [k: string]: unknown;
}

export interface TokenConversationSummary {
  key: string;
  sessionIds?: string[];
  sessions?: number;
  buckets?: TokenBucketsBreakdown;
  cost: number;
  unattributedCost?: number;
  attributedCost?: number;
  byTier?: Record<string, { cost?: number; tokens?: number; [k: string]: unknown }>;
  turns: number;
  steps: number;
  estimated?: boolean;
  lastAt?: number | null;
  firstAt?: number | null;
  models?: Record<string, unknown>;
}

export interface TokenSummaryResponse {
  ok?: boolean;
  generatedAt?: number;
  priceVersion?: string;
  currency?: string;
  symbol?: string;
  nowTier?: RateInfo | null;
  priceTable?: {
    version?: string;
    currency?: string;
    symbol?: string;
    unit?: number;
    peakMultiplier?: number;
    peakWindows?: [number, number][];
    peakWeekdays?: number[];
    holidayCount?: number;
    models?: Record<string, unknown>;
  };
  totals: {
    buckets?: TokenBucketsBreakdown;
    cost: number;
    attributedCost?: number;
    unattributedCost?: number;
    byTier?: Record<string, unknown>;
    turns: number;
    steps: number;
    conversations: number;
    estimated?: boolean;
    totalTokens?: number;
    inputTokens?: number;
    cacheHitRate?: number | null;
    avgCostPerTurn?: number | null;
    avgCostPerStep?: number | null;
  };
  conversations: TokenConversationSummary[];
  ledger?: Record<string, unknown>;
}

export interface TokenProviderModelRow {
  model: string;
  samples: number;
  cost: number;
}

export interface TokenProviderRow {
  provider: string;
  samples: number;
  tokens?: {
    in?: number;
    out?: number;
    reasoning?: number;
    cacheHit?: number;
    cacheMiss?: number;
    cacheWrite?: number;
    total?: number;
  };
  cost: number;
  byTier?: Record<string, unknown>;
  estimated?: boolean;
  models?: TokenProviderModelRow[];
}

export interface TokensByProviderResponse {
  ok?: boolean;
  providers: TokenProviderRow[];
  totalCost: number;
  totalSamples: number;
}

export interface UsageAttributionSlice {
  turns?: number;
  tokens?: number;
  cost?: number;
  samples?: number;
  events?: number;
  note?: string;
}

export interface UsageAttributionResponse {
  ok?: boolean;
  days: number;
  symbol?: string;
  window?: { fromDay?: string; toDay?: string };
  our?: UsageAttributionSlice;
  dshTotal?: UsageAttributionSlice;
  unattributed?: UsageAttributionSlice;
  byKey?: { key: string; turns?: number; tokens?: number; cost?: number }[];
  byProvider?: { provider: string; turns?: number; tokens?: number; cost?: number }[];
}

export interface TokenTurnRow {
  sessionId?: string;
  turn: number | string;
  steps?: number;
  attempts?: number;
  startedAt?: number;
  endedAt?: number;
  buckets?: TokenBucketsBreakdown;
  cost: number;
  tier?: string;
  key?: string;
  model?: string;
  provider?: string;
}

export interface TokenTurnsResponse {
  ok?: boolean;
  key: string;
  generatedAt?: number;
  currency?: string;
  symbol?: string;
  turnLimit?: number;
  turns: TokenTurnRow[];
}

export interface TokenRecentEntry {
  time?: number;
  key?: string;
  sessionId?: string;
  turn?: number | string;
  step?: number;
  retry?: number;
  model?: string;
  tokens?: number;
  cacheMiss?: number;
  cacheHit?: number;
  cacheWrite?: number;
  output?: number;
  reasoning?: number;
  cost?: number;
  tier?: string;
  estimated?: boolean;
}

export interface SettingsHistoryEntry {
  entry: number;
  atMs: number;
  actor: string;
  source: string;
  scope: string;
  setting: string;
  old: unknown;
  new: unknown;
}

export interface SettingsHistoryResponse {
  ok?: boolean;
  entries: SettingsHistoryEntry[];
  next: number | null;
}

export async function fetchSocialV2Activity(): Promise<{ ok?: boolean; paused: boolean }> {
  return apiRequest('/api/socialV2/activity');
}

export async function resetSocialV2(): Promise<{ ok?: boolean }> {
  return apiRequest('/api/socialV2/reset', {
    method: 'POST',
    body: {},
  });
}

export async function fetchSocialV2Feedback(): Promise<{
  ok?: boolean;
  entries: SocialV2FeedbackEntry[];
}> {
  return apiRequest('/api/socialV2/feedback');
}

export async function clearSocialV2Feedback(): Promise<{ ok?: boolean }> {
  return apiRequest('/api/socialV2/feedback-clear', {
    method: 'POST',
    body: {},
  });
}

export async function fetchToolLog(
  limit = 200,
): Promise<{ ok?: boolean; entries: ToolLogEntry[] }> {
  const clamped = Math.max(1, Math.min(500, Math.round(limit)));
  return apiRequest(`/api/socialV2/tool-log?limit=${clamped}`);
}

export async function clearToolLog(): Promise<{ ok?: boolean }> {
  return apiRequest('/api/socialV2/tool-log/clear', {
    method: 'POST',
    body: {},
  });
}

export async function fetchDetailedModelRoutes(): Promise<DetailedModelsResponse> {
  return apiRequest('/api/settings/models');
}

export async function fetchLegacyModelChain(
  conversationKey?: string,
): Promise<LegacyModelChainResponse> {
  const query = conversationKey ? `?key=${encodeURIComponent(conversationKey)}` : '';
  return apiRequest(`/api/model-chain${query}`);
}

export async function fetchTokenSeries(
  hours: 24 | 72 | 168 | 720,
  buckets = 48,
): Promise<TokenSeriesResponse> {
  return apiRequest(`/api/tokens/series?hours=${hours}&buckets=${buckets}`);
}

export async function fetchTokenSummary(limit = 200): Promise<TokenSummaryResponse> {
  return apiRequest(`/api/tokens/summary?limit=${limit}`);
}

export async function fetchTokensByProvider(): Promise<TokensByProviderResponse> {
  return apiRequest('/api/tokens/by-provider');
}

export async function fetchUsageAttribution(days: number): Promise<UsageAttributionResponse> {
  const clamped = Math.max(1, Math.min(90, Math.round(days)));
  return apiRequest(`/api/usage/attribution?days=${clamped}`);
}

export async function fetchTokenTurns(
  conversationKey: string,
  limit = 200,
): Promise<TokenTurnsResponse> {
  return apiRequest(
    `/api/tokens/turns?key=${encodeURIComponent(conversationKey)}&limit=${limit}`,
  );
}

export async function fetchTokenRecent(
  limit = 50,
): Promise<{ ok?: boolean; symbol?: string; entries: TokenRecentEntry[] }> {
  return apiRequest(`/api/tokens/recent?limit=${limit}`);
}

export async function resetTokenLedger(): Promise<{ ok?: boolean }> {
  return apiRequest('/api/tokens/reset', {
    method: 'POST',
    body: {},
  });
}

export async function fetchGlobalSettingsHistory(params: {
  setting?: string;
  conversation?: string;
  before?: number | null;
}): Promise<SettingsHistoryResponse> {
  const search = new URLSearchParams();
  if (params.setting?.trim()) {
    search.set('setting', params.setting.trim());
  }
  if (params.conversation?.trim()) {
    search.set('conversation', params.conversation.trim());
  }
  if (typeof params.before === 'number' && Number.isFinite(params.before)) {
    search.set('before', String(params.before));
  }
  const qs = search.toString();
  return apiRequest(`/api/settings/history${qs ? `?${qs}` : ''}`);
}

export async function undoGlobalSetting(
  entry: number,
  confirm = false,
): Promise<{
  ok?: boolean;
  entry?: number;
  setting?: string;
  stored?: unknown;
  conversation?: string | null;
  mirrorError?: string | null;
  wakeRefreshError?: string | null;
}> {
  return apiRequest('/api/settings/undo', {
    method: 'POST',
    body: { entry, confirm },
  });
}
