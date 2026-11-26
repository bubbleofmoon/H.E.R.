import { ApiRequestError, apiRequest } from './client';

export interface SocialV1Config {
  enabled: boolean;
  triggerProbability: number;
  contextWindow: number;
  activeCheckMinMs: number;
  activeCheckMaxMs: number;
  activeReplyDelayMinMs: number;
  activeReplyDelayMaxMs: number;
  activeDurationEnabled: boolean;
  activeDurationMinMs: number;
  activeDurationMaxMs: number;
  idleWindowMs: number;
  idleRetryProbability: number;
  idleRetryWaitMs: number;
  proactiveEnabled: boolean;
  proactiveIdleThresholdMs: number;
  proactiveCheckMinMs: number;
  proactiveCheckMaxMs: number;
  proactiveProbability: number;
  skipProbability: number;
  surrenderProbability: number;
  maxReplyChars: number;
  mustReplyKeywords: string[];
  burstEnabled: boolean;
  burstIntervalMinMs: number;
  burstIntervalMaxMs: number;
  longGapProbability: number;
  longGapMinMs: number;
  longGapMaxMs: number;
}

export interface SocialV1Snapshot {
  config: SocialV1Config;
  pendingSummaries: Record<string, number>;
  states: Record<string, { phase?: string }>;
}

export interface DshEffortLabel {
  name?: string;
  description?: string;
}

export interface DshModelInfo {
  provider?: string;
  model?: string;
  reasoningEffort?: string | null;
  options?: string[];
  labels?: Record<string, DshEffortLabel | string>;
  default?: string;
  providerDefault?: string | null;
  catalogSource?: string;
  globalSideEffect?: boolean;
}

export type LegacyAvailability<T> =
  | { available: true; data: T }
  | { available: false; status: number; reason: string };

async function optionalDshRequest<T>(
  path: string,
  method: 'GET' | 'POST',
  body?: unknown,
): Promise<LegacyAvailability<T>> {
  try {
    const data = await apiRequest<T>(path, { method, body });
    return { available: true, data };
  } catch (err) {
    if (err instanceof ApiRequestError && (err.status === 404 || err.status === 503)) {
      return { available: false, status: err.status, reason: err.message };
    }
    throw err;
  }
}

export async function fetchSocialV1(): Promise<SocialV1Snapshot> {
  return apiRequest<SocialV1Snapshot>('/api/social', { method: 'GET' });
}

export async function saveSocialV1(
  config: Partial<SocialV1Config>,
): Promise<{ ok: boolean; config: SocialV1Config }> {
  return apiRequest<{ ok: boolean; config: SocialV1Config }>('/api/social', {
    method: 'POST',
    body: config,
  });
}

export async function flushSocialV1(key?: string): Promise<{ ok: boolean }> {
  return apiRequest<{ ok: boolean }>('/api/social/flush', {
    method: 'POST',
    body: key ? { key } : {},
  });
}

export async function setSocialV1Phase(
  key: string,
  phase: 'active' | 'idle',
): Promise<{ ok: boolean; key: string; phase: string }> {
  return apiRequest<{ ok: boolean; key: string; phase: string }>('/api/social/state', {
    method: 'POST',
    body: { key, phase },
  });
}

export function fetchDshModel(): Promise<LegacyAvailability<DshModelInfo>> {
  return optionalDshRequest<DshModelInfo>('/api/dsh/model', 'GET');
}

export function saveDshEffort(
  reasoningEffort: string,
): Promise<LegacyAvailability<{ ok: boolean; reasoningEffort: string }>> {
  return optionalDshRequest<{ ok: boolean; reasoningEffort: string }>('/api/dsh/effort', 'POST', {
    reasoningEffort,
  });
}

export function notifyDsh(
  key: string,
  message: string,
): Promise<LegacyAvailability<Record<string, unknown>>> {
  return optionalDshRequest<Record<string, unknown>>('/api/console/notify-ai', 'POST', {
    key,
    message,
  });
}
