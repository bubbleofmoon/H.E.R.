import { apiRequest } from './client';
export {
  appendLightMemory,
  clearLightMemory,
  fetchLightMemory,
  fetchPersonasList,
  removeLightMemory,
  updateLightMemory,
  type LightMemorySnapshot,
  type PersonaListResponse,
} from './run';

export const PERSONA_UPDATED_EVENT = 'her:persona-updated';

export function emitPersonaUpdated(): void {
  if (typeof window !== 'undefined') {
    window.dispatchEvent(new CustomEvent(PERSONA_UPDATED_EVENT));
  }
}

export type PersonaAutoPart = 'core' | 'notes' | 'compact' | 'names' | 'traits';

export interface PersonaAutoRules {
  parts: PersonaAutoPart[];
  perWeek: number;
  monthlyDriftPause: boolean;
  notify: boolean;
}

export interface PersonaVersionDetail {
  version: number;
  createdAt: number;
  source: string;
  approved: boolean;
  approvedAt: number | null;
  name: string;
  core: string;
  notes: string;
  compact: string;
  names: string[];
  traits: { neediness: number };
  editMode: 'auto' | 'manual';
  autoRules: PersonaAutoRules;
  coreTokens: number;
  compactTokens: number;
}

export interface PersonaItemResponse {
  ok: boolean;
  persona: {
    id: string;
    activeVersion: number | null;
    versions: PersonaVersionDetail[];
  };
  budgets: {
    coreTokens: number;
    compactTokens: number;
  };
}

export interface PersonaCreatePayload {
  name: string;
  core: string;
  notes?: string;
  compact?: string;
  names?: string[];
  traits?: { neediness: number };
  editMode?: 'auto' | 'manual';
  autoRules?: PersonaAutoRules;
}

export interface PersonaUpdatePayload extends PersonaCreatePayload {
  id: string;
}

export interface PersonaUpdateResponse {
  ok: boolean;
  id: string;
  version: number;
  activeVersion: number | null;
  unchanged?: boolean;
}

export interface PersonaAssignResponse {
  ok: boolean;
  default: string | null;
  overrides: Record<string, string>;
  rebriefed: string[];
  rebriefFailed: { conversation: string; error: string }[];
}

export interface WakeWordsLogEntry {
  id: number;
  at: number;
  by: 'her' | 'console' | 'undo' | string;
  undoes?: number;
  added: { keywords: string[]; dropNames: string[] };
  removed: { keywords: string[]; dropNames: string[] };
}

export interface WakeWordsResponse {
  ok: boolean;
  conversation: string;
  personaNames: string[];
  keywords: string[];
  dropNames: string[];
  effective: string[];
  log: WakeWordsLogEntry[];
}

export interface LegacyRolesSummaryResponse {
  roles: string[];
  summaries: {
    name: string;
    bytes: number;
    chars: number;
    lines: number;
    mtime: number;
    excerpt: string;
    injectedV1: number;
    injectedV2: number;
    hasModeTags: boolean;
    warnings: number;
  }[];
  current: string | null;
  limits: {
    nameMax: number;
    contentMaxBytes: number;
    injectMaxChars: number;
  };
}

export type SimPromptPreset = 'qq-chat-v2' | 'qq-chat';

export interface SimPromptStatus {
  preset: SimPromptPreset;
  label: string;
  sourceFile: string;
  installed: boolean;
  requiresRestart: boolean;
}

export interface SimPromptResponse {
  ok: boolean;
  content: string;
  chars: number;
  bytes: number;
  warnings: string[];
  status: SimPromptStatus;
  presets: SimPromptStatus[];
}

export interface SimPromptBackupItem {
  file: string;
  mtime: number;
}

export interface SocialV2PromptPreview {
  ok: boolean;
  key: string;
  time: string;
  self: { nickname: string | null; userId: string | null; line: string | null };
  role: { name: string | null; hint: string };
  recommended: Record<string, unknown> | null;
  enabledTools: string[];
  replyTiming: Record<string, unknown>;
  unreadCount: number;
  recentCount: number;
  currentWakeConfig: Record<string, unknown>;
  wakeSafety: Record<string, unknown>;
  memory: string;
  participation: string;
  slang: { enabled: boolean; entries: unknown[]; block: string };
  stickers: { enabled: boolean; total: number; context: string; strategy: string };
  voices: { enabled: boolean; total?: number; names?: string[]; maxSeconds?: number; hint?: string };
}

export interface LongTermMemoryHit {
  id: number;
  owner_id: string;
  scope: string;
  kind: string;
  subject: string | null;
  content: string;
  score: number;
  tags: string | null;
  source: string;
  importance: number;
  pinned: number;
  hits: number;
  created_at: number;
  last_used_at: number | null;
  owner_type: string;
  archived: number;
  expires_at: number | null;
  provenance: 'fact' | 'inference';
  message_id: string | null;
  attributed_to: string | null;
}

export type SlangStatus = 'candidate' | 'confirmed' | 'rejected';

export interface SlangEvidenceItem {
  key: string;
  sender: string;
  text: string;
  time: number;
}

export interface SlangEntry {
  id: string;
  content: string;
  meaning: string;
  usage: string;
  example: string;
  risk: string;
  sources: string[];
  status: SlangStatus;
  source: 'manual' | 'ai' | string;
  count: number;
  evidence: SlangEvidenceItem[];
  lastInferenceCount: number;
  createdAt: string;
  updatedAt: string;
}

export interface SlangConfig {
  enabled: boolean;
  extractMinMessages: number;
  extractCooldownMs: number;
  inferenceThresholds: number[];
  injectMax: number;
  autoResearch: boolean;
  learnerFallbackDsh: boolean;
}

export interface SlangListResponse {
  entries: SlangEntry[];
  config: SlangConfig;
  writable: boolean;
}

export interface StickerView {
  id: string;
  resId: string;
  md5: string;
  url: string;
  desc: string;
  localNote: string;
  tags: string[];
  usage: string;
  useCount: number;
  lastUsedAt: number;
  source: string;
}

export interface StickerImageFrame {
  mimeType: string;
  data: string;
  frameIndex?: number;
  text?: string;
  originalMimeType?: string;
}

export interface StickerImageResponse {
  ok: boolean;
  key: string;
  sticker: {
    id: string;
    desc: string;
    localNote: string;
    tags: string[];
    url: string;
    md5: string;
  };
  image: StickerImageFrame;
  images: StickerImageFrame[];
}

export interface VoiceFileEntry {
  name: string;
  ok?: boolean;
  bytes?: number;
  detail?: string | null;
  seconds?: number | null;
  error?: string;
}

export interface VoiceListResponse {
  ok: boolean;
  key: string | null;
  count: number;
  voices: VoiceFileEntry[];
  limits: {
    maxSeconds: number;
    maxBytes: number;
    maxPerMinute: number;
    maxPerHour: number;
  };
  hint: string;
  dir?: string;
}

export interface VoiceSendResponse {
  ok: boolean;
  dryRun?: boolean;
  key: string;
  file: string;
  via: string;
  bytes: number;
  duration: number;
  codec: string;
  detail: string;
  warnings: string[];
  messageId?: number | null;
  verified?: boolean;
  readback?: string | null;
  audioProcessing?: string;
  volume?: number;
  loudness?: number | null;
}

export async function fetchPersonaItem(id: string): Promise<PersonaItemResponse> {
  return apiRequest<PersonaItemResponse>(`/api/personas/item?id=${encodeURIComponent(id)}`, {
    method: 'GET',
  });
}

export async function createPersona(
  payload: PersonaCreatePayload,
): Promise<{ ok: boolean; id: string; version: number }> {
  return apiRequest('/api/personas/create', { method: 'POST', body: payload });
}

export async function updatePersona(
  payload: PersonaUpdatePayload,
): Promise<PersonaUpdateResponse> {
  return apiRequest<PersonaUpdateResponse>('/api/personas/update', {
    method: 'POST',
    body: payload,
  });
}

export async function duplicatePersona(
  id: string,
  name: string,
): Promise<{ ok: boolean; id: string; version: number }> {
  return apiRequest('/api/personas/duplicate', { method: 'POST', body: { id, name } });
}

export async function deletePersona(id: string): Promise<{ ok: boolean; id: string }> {
  return apiRequest('/api/personas/delete', { method: 'POST', body: { id } });
}

export async function approvePersonaVersion(
  id: string,
  version: number,
): Promise<{ ok: boolean; id: string; activeVersion: number }> {
  return apiRequest('/api/personas/approve', { method: 'POST', body: { id, version } });
}

export async function assignPersonaDefault(
  defaultId: string | null,
): Promise<PersonaAssignResponse> {
  return apiRequest<PersonaAssignResponse>('/api/personas/assign', {
    method: 'POST',
    body: { default: defaultId },
  });
}

export async function importRoleCards(): Promise<{
  ok: boolean;
  imported: { id: string; name: string }[];
  skipped: string[];
}> {
  return apiRequest('/api/personas/import-roles', { method: 'POST', body: {} });
}

export async function fetchWakeWords(conversation: string): Promise<WakeWordsResponse> {
  return apiRequest<WakeWordsResponse>(
    `/api/personas/wake-words?conversation=${encodeURIComponent(conversation)}`,
    { method: 'GET' },
  );
}

export async function undoWakeWords(
  conversation: string,
  id: number,
): Promise<{
  ok: boolean;
  conversation: string;
  id: number;
  undid: number;
  wakeWords: { keywords: string[]; dropNames: string[] };
}> {
  return apiRequest('/api/personas/wake-words/undo', {
    method: 'POST',
    body: { conversation, id },
  });
}

export async function fetchRoles(): Promise<LegacyRolesSummaryResponse> {
  return apiRequest<LegacyRolesSummaryResponse>('/api/roles', { method: 'GET' });
}

export async function updateRoleInjectLimit(maxInjectChars: number): Promise<{
  ok: boolean;
  maxInjectChars: number;
  limits: { injectMaxChars: number };
}> {
  return apiRequest('/api/roles/limit', {
    method: 'POST',
    body: { maxInjectChars },
  });
}

export async function fetchSimPrompt(preset?: SimPromptPreset): Promise<SimPromptResponse> {
  const qs = preset ? `?preset=${encodeURIComponent(preset)}` : '';
  return apiRequest<SimPromptResponse>(`/api/preset/sim-prompt${qs}`, { method: 'GET' });
}

export async function saveSimPrompt(
  preset: SimPromptPreset,
  content: string,
): Promise<{
  ok: boolean;
  unchanged?: boolean;
  chars: number;
  warnings: string[];
  synced: boolean;
  requiresRestart: boolean;
  status: SimPromptStatus;
}> {
  return apiRequest('/api/preset/sim-prompt', {
    method: 'POST',
    body: { preset, content },
  });
}

export async function fetchSimPromptBackups(
  preset?: SimPromptPreset,
): Promise<{ ok: boolean; backups: SimPromptBackupItem[] }> {
  const qs = preset ? `?preset=${encodeURIComponent(preset)}` : '';
  return apiRequest(`/api/preset/sim-prompt/backups${qs}`, { method: 'GET' });
}

export async function restoreSimPrompt(
  preset: SimPromptPreset,
  file?: string,
): Promise<{ ok: boolean; restoredFrom: string; requiresRestart: boolean }> {
  return apiRequest('/api/preset/sim-prompt/restore', {
    method: 'POST',
    body: file ? { preset, file } : { preset },
  });
}

export async function fetchSocialV2Prompt(key: string): Promise<SocialV2PromptPreview> {
  return apiRequest<SocialV2PromptPreview>(
    `/api/socialV2/prompt?key=${encodeURIComponent(key)}`,
    { method: 'GET' },
  );
}

export async function archiveMemory(
  id: number,
  archived = true,
): Promise<{ ok: boolean; changed: boolean; id: number; archived: boolean }> {
  return apiRequest('/api/memory/archive', {
    method: 'POST',
    body: { id, archived },
  });
}

export async function searchMemories(
  key: string,
  query = '',
  kinds?: string,
  limit = 20,
): Promise<{ ok: boolean; count: number; memories: LongTermMemoryHit[] }> {
  return apiRequest('/api/memory/search', {
    method: 'POST',
    body: kinds ? { key, query, kinds, limit } : { key, query, limit },
  });
}

export async function writeMemory(payload: {
  key: string;
  content: string;
  kind?: string;
  scope?: string;
  subject?: string;
  tags?: string;
  importance?: number;
}): Promise<{ ok: boolean; id: number; deduped: boolean; hash: string }> {
  return apiRequest('/api/memory/write', { method: 'POST', body: payload });
}

export async function forgetMemory(key: string, id: number): Promise<{ ok: boolean }> {
  return apiRequest('/api/memory/forget', { method: 'POST', body: { key, id } });
}

export async function previewMemory(
  key: string,
): Promise<{ ok: boolean; chars: number; block: string }> {
  return apiRequest('/api/memory/preview', { method: 'POST', body: { key } });
}

export async function fetchMemoryIndex(
  key: string,
): Promise<{ ok: boolean; total: number; terms: string[] }> {
  return apiRequest('/api/memory/index', { method: 'POST', body: { key } });
}

export async function fetchSlang(status?: SlangStatus): Promise<SlangListResponse> {
  const qs = status ? `?status=${encodeURIComponent(status)}` : '';
  return apiRequest<SlangListResponse>(`/api/slang${qs}`, { method: 'GET' });
}

export async function addSlang(payload: {
  content: string;
  meaning?: string;
  usage?: string;
  example?: string;
  risk?: string;
  sources?: string[];
  status?: 'candidate' | 'confirmed';
}): Promise<{ ok: boolean; entry: SlangEntry }> {
  return apiRequest('/api/slang', { method: 'POST', body: payload });
}

export async function patchSlang(
  id: string,
  payload: {
    content?: string;
    meaning?: string;
    usage?: string;
    example?: string;
    risk?: string;
    sources?: string[];
    status?: SlangStatus;
  },
): Promise<{ ok: boolean; entry: SlangEntry }> {
  return apiRequest(`/api/slang/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    body: payload,
  });
}

export async function deleteSlang(id: string): Promise<{ ok: boolean; removed: SlangEntry }> {
  return apiRequest(`/api/slang/${encodeURIComponent(id)}`, {
    method: 'DELETE',
    body: {},
  });
}

export async function confirmSlang(
  id: string,
  edits?: {
    meaning?: string;
    usage?: string;
    example?: string;
    risk?: string;
    sources?: string[];
  },
): Promise<{ ok: boolean; entry: SlangEntry }> {
  return apiRequest(`/api/slang/${encodeURIComponent(id)}/confirm`, {
    method: 'POST',
    body: edits ?? {},
  });
}

export async function rejectSlang(id: string): Promise<{ ok: boolean; entry: SlangEntry }> {
  return apiRequest(`/api/slang/${encodeURIComponent(id)}/reject`, {
    method: 'POST',
    body: {},
  });
}

export async function batchConfirmSlang(ids: string[]): Promise<{
  ok: boolean;
  confirmedCount: number;
  skippedCount: number;
  skipped: { id: string; content: string; reason: string }[];
}> {
  return apiRequest('/api/slang/batch-confirm', { method: 'POST', body: { ids } });
}

export async function batchRejectSlang(
  ids: string[],
): Promise<{ ok: boolean; rejectedCount: number }> {
  return apiRequest('/api/slang/batch-reject', { method: 'POST', body: { ids } });
}

export async function batchDeleteSlang(
  payload: { ids: string[] } | { status: SlangStatus },
): Promise<{ ok: boolean; removedCount: number }> {
  return apiRequest('/api/slang/batch-delete', { method: 'POST', body: payload });
}

export async function clearSlang(status?: SlangStatus): Promise<{ ok: boolean; removedCount: number }> {
  return apiRequest('/api/slang/clear', {
    method: 'POST',
    body: status ? { status } : {},
  });
}

export async function researchSlang(ids?: string[]): Promise<{ ok: boolean; count: number }> {
  return apiRequest('/api/slang/research', {
    method: 'POST',
    body: ids && ids.length > 0 ? { ids } : {},
  });
}

export async function extractSlang(key?: string): Promise<{ ok: boolean; key: string }> {
  return apiRequest('/api/slang/extract', {
    method: 'POST',
    body: key ? { key } : {},
  });
}

export async function saveSlangConfig(
  config: Partial<SlangConfig>,
): Promise<{ ok: boolean; config: SlangConfig }> {
  return apiRequest('/api/slang/config', { method: 'POST', body: config });
}

export async function fetchStickers(
  query = '',
  count = 48,
): Promise<{
  ok: boolean;
  total: number;
  matched: number;
  truncated: boolean;
  stickers: StickerView[];
  syncedAt: number;
  fromCache: boolean;
  writable: boolean;
}> {
  const params = new URLSearchParams();
  if (query.trim()) params.set('query', query.trim());
  params.set('count', String(count));
  return apiRequest(`/api/stickers?${params.toString()}`, { method: 'GET' });
}

export async function syncStickers(): Promise<{ ok: boolean; total: number; syncedAt: number }> {
  return apiRequest('/api/stickers/sync', { method: 'POST', body: {} });
}

export async function saveStickerNote(payload: {
  stickerId: string;
  note?: string | null;
  usage?: string | null;
  tags?: string[] | null;
}): Promise<{ ok: boolean; sticker: StickerView }> {
  return apiRequest('/api/stickers/note', { method: 'POST', body: payload });
}

export async function saveStickerRemark(
  stickerId: string,
  remark: string,
): Promise<{ ok: boolean; sticker: StickerView }> {
  return apiRequest('/api/stickers/remark', {
    method: 'POST',
    body: { stickerId, remark },
  });
}

export async function fetchStickerImage(
  key: string,
  stickerId: string,
): Promise<StickerImageResponse> {
  return apiRequest<StickerImageResponse>(
    `/api/socialV2/sticker-image?key=${encodeURIComponent(key)}&stickerId=${encodeURIComponent(stickerId)}`,
    { method: 'GET' },
  );
}

export async function collectSticker(payload: {
  key: string;
  messageId?: string;
  seq?: string;
  remark?: string;
}): Promise<{ ok: boolean; sticker: StickerView | null; emojiId: string; remark: string }> {
  return apiRequest('/api/socialV2/collect-sticker', { method: 'POST', body: payload });
}

export async function fetchVoices(probe = true): Promise<VoiceListResponse> {
  return apiRequest<VoiceListResponse>(`/api/voice/list?probe=${probe ? '1' : '0'}`, {
    method: 'GET',
  });
}

export async function sendVoice(payload: {
  target: string;
  file: string;
  volume?: number;
  loudness?: 'quiet' | 'normal' | 'loud' | number;
  normalize?: boolean;
  dryRun?: boolean;
  verify?: boolean;
  readback?: boolean;
}): Promise<VoiceSendResponse> {
  return apiRequest<VoiceSendResponse>('/api/voice/send', {
    method: 'POST',
    body: payload,
  });
}
