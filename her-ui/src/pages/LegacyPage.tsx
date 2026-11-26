import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Plus, RefreshCw, Send, Sparkles, Terminal, X } from 'lucide-react';
import {
  DshEffortLabel,
  DshModelInfo,
  LegacyAvailability,
  SocialV1Config,
  SocialV1Snapshot,
  fetchDshModel,
  fetchSocialV1,
  flushSocialV1,
  notifyDsh,
  saveDshEffort,
  saveSocialV1,
  setSocialV1Phase,
} from '../api/legacy';
import {
  Badge,
  BadgeTone,
  Button,
  Card,
  CardHeader,
  PageHeader,
  Switch,
} from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';

type BooleanSocialKey =
  | 'enabled'
  | 'activeDurationEnabled'
  | 'burstEnabled'
  | 'proactiveEnabled';

type NumericSocialKey = Exclude<keyof SocialV1Config, BooleanSocialKey | 'mustReplyKeywords'>;

const BOOLEAN_KEYS: readonly BooleanSocialKey[] = [
  'enabled',
  'activeDurationEnabled',
  'burstEnabled',
  'proactiveEnabled',
];

const DEFAULT_SOCIAL_V1_CONFIG: SocialV1Config = {
  enabled: true,
  triggerProbability: 0.1,
  contextWindow: 20,
  activeCheckMinMs: 10000,
  activeCheckMaxMs: 30000,
  activeReplyDelayMinMs: 2000,
  activeReplyDelayMaxMs: 8000,
  activeDurationEnabled: true,
  activeDurationMinMs: 900000,
  activeDurationMaxMs: 1800 * 1000,
  idleWindowMs: 360000,
  idleRetryProbability: 0.25,
  idleRetryWaitMs: 120000,
  proactiveEnabled: true,
  proactiveIdleThresholdMs: 1800 * 1000,
  proactiveCheckMinMs: 2700 * 1000,
  proactiveCheckMaxMs: 5400 * 1000,
  proactiveProbability: 0.2,
  skipProbability: 0.15,
  surrenderProbability: 0,
  maxReplyChars: 500,
  mustReplyKeywords: ['deepseek', '小鲸鱼', '大肥鱼', '鲸鱼', 'd指导', '在吗'],
  burstEnabled: true,
  burstIntervalMinMs: 1000,
  burstIntervalMaxMs: 3000,
  longGapProbability: 0.1,
  longGapMinMs: 2500,
  longGapMaxMs: 5000,
};

const DEFAULT_EFFORT_OPTIONS = ['low', 'medium', 'high', 'max'] as const;

const DEFAULT_EFFORT_LABELS: Record<string, string> = {
  low: 'low（低）',
  medium: 'medium（中）',
  high: 'high（高）',
  max: 'max（最高）',
};

const DSH_MCP_TOOL_REFERENCE: { name: string; purpose: string }[] = [
  { name: 'qq_status', purpose: '只读查看当前 QQ 连接态及本号基础资料' },
  { name: 'qq_list_groups', purpose: '列出所在 QQ 群的群号与群名（只读）' },
  { name: 'qq_get_group_members', purpose: '列出指定 QQ 群成员的 QQ 号、昵称与群名片（只读）' },
  { name: 'qq_get_group_history', purpose: '按可选 messageSeq 读取指定 QQ 群的最近历史消息（只读）' },
  {
    name: 'qq_send_group_message',
    purpose: '向允许名单里的指定群聊投递纯文本发言，也可附带引用消息编号进行回应',
  },
  {
    name: 'qq_reply',
    purpose: '针对允许名单群聊里的某条特定消息发出引用回应，并附带纯文本正文内容',
  },
  {
    name: 'qq_send_private_message',
    purpose: '向私聊允许名单里的指定好友递出纯文本私信，也可附带引用编号回应对方',
  },
  { name: 'qq_slang_query', purpose: '只读查询当前已确认的群黑话、梗与常用表达' },
  { name: 'qq_slang_submit', purpose: '将群内遇到的陌生词或黑话提交至候选库供管理员审核' },
];

interface NumberFieldSpec {
  key: NumericSocialKey;
  label: string;
  step?: string;
  min?: number;
  max?: number;
  integer?: boolean;
}

const RESERVED1_FIELDS: NumberFieldSpec[] = [
  { key: 'triggerProbability', label: '触发概率 (0-1)', step: '0.01', min: 0, max: 1 },
  { key: 'contextWindow', label: '上下文窗口条数 (1-100)', step: '1', min: 1, max: 100, integer: true },
  { key: 'activeCheckMinMs', label: '活跃检查最小间隔 (ms)', step: '100', min: 0, integer: true },
  { key: 'activeCheckMaxMs', label: '活跃检查最大间隔 (ms)', step: '100', min: 0, integer: true },
  { key: 'activeReplyDelayMinMs', label: '活跃回复最小延迟 (ms)', step: '100', min: 0, integer: true },
  { key: 'activeReplyDelayMaxMs', label: '活跃回复最大延迟 (ms)', step: '100', min: 0, integer: true },
  { key: 'activeDurationMinMs', label: '活跃持续最小时长 (ms)', step: '100', min: 0, integer: true },
  { key: 'activeDurationMaxMs', label: '活跃持续最大时长 (ms)', step: '100', min: 0, integer: true },
  { key: 'idleWindowMs', label: '进入空闲窗口 (ms)', step: '1000', min: 0, integer: true },
  { key: 'idleRetryProbability', label: '空闲重试概率 (0-1)', step: '0.01', min: 0, max: 1 },
  { key: 'idleRetryWaitMs', label: '空闲重试等待 (ms)', step: '1000', min: 0, integer: true },
  { key: 'burstIntervalMinMs', label: '分条间隔最小 (ms)', step: '100', min: 0, integer: true },
  { key: 'burstIntervalMaxMs', label: '分条间隔最大 (ms)', step: '100', min: 0, integer: true },
  { key: 'longGapProbability', label: '长停顿概率 (0-1)', step: '0.01', min: 0, max: 1 },
  { key: 'longGapMinMs', label: '长停顿最小 (ms)', step: '100', min: 0, integer: true },
  { key: 'longGapMaxMs', label: '长停顿最大 (ms)', step: '100', min: 0, integer: true },
  { key: 'proactiveIdleThresholdMs', label: '主动开口空闲阈值 (ms)', step: '1000', min: 0, integer: true },
  { key: 'proactiveCheckMinMs', label: '主动检查最小间隔 (ms)', step: '1000', min: 0, integer: true },
  { key: 'proactiveCheckMaxMs', label: '主动检查最大间隔 (ms)', step: '1000', min: 0, integer: true },
  { key: 'proactiveProbability', label: '主动开话题概率 (0-1)', step: '0.01', min: 0, max: 1 },
  { key: 'skipProbability', label: '跳过回复概率 (0-1)', step: '0.01', min: 0, max: 1 },
  { key: 'surrenderProbability', label: '让出话题概率 (0-1)', step: '0.01', min: 0, max: 1 },
  { key: 'maxReplyChars', label: '单次回复最大字符数 (0 默认 500)', step: '10', min: 0, integer: true },
];

function createFieldGenMap(): Record<keyof SocialV1Config, number> {
  const gen = {
    enabled: 0,
    activeDurationEnabled: 0,
    burstEnabled: 0,
    proactiveEnabled: 0,
    mustReplyKeywords: 0,
  } as Record<keyof SocialV1Config, number>;
  for (const f of RESERVED1_FIELDS) {
    gen[f.key] = 0;
  }
  return gen;
}

function parseLegacyNumber(v: unknown, fallback: number): number {
  if (typeof v === 'number') {
    return Number.isFinite(v) ? v : fallback;
  }
  if (typeof v === 'string') {
    const trimmed = v.trim();
    if (trimmed === '') return 0;
    const parsed = Number(trimmed);
    return Number.isFinite(parsed) ? parsed : fallback;
  }
  return fallback;
}

function normalizeSocialConfig(raw?: Partial<SocialV1Config> | null): SocialV1Config {
  const src = raw ?? {};
  const bool = (v: unknown, fb: boolean) => (typeof v === 'boolean' ? v : fb);
  return {
    enabled: bool(src.enabled, DEFAULT_SOCIAL_V1_CONFIG.enabled),
    triggerProbability: parseLegacyNumber(
      src.triggerProbability,
      DEFAULT_SOCIAL_V1_CONFIG.triggerProbability,
    ),
    contextWindow: parseLegacyNumber(src.contextWindow, DEFAULT_SOCIAL_V1_CONFIG.contextWindow),
    activeCheckMinMs: parseLegacyNumber(
      src.activeCheckMinMs,
      DEFAULT_SOCIAL_V1_CONFIG.activeCheckMinMs,
    ),
    activeCheckMaxMs: parseLegacyNumber(
      src.activeCheckMaxMs,
      DEFAULT_SOCIAL_V1_CONFIG.activeCheckMaxMs,
    ),
    activeReplyDelayMinMs: parseLegacyNumber(
      src.activeReplyDelayMinMs,
      DEFAULT_SOCIAL_V1_CONFIG.activeReplyDelayMinMs,
    ),
    activeReplyDelayMaxMs: parseLegacyNumber(
      src.activeReplyDelayMaxMs,
      DEFAULT_SOCIAL_V1_CONFIG.activeReplyDelayMaxMs,
    ),
    activeDurationEnabled: bool(
      src.activeDurationEnabled,
      DEFAULT_SOCIAL_V1_CONFIG.activeDurationEnabled,
    ),
    activeDurationMinMs: parseLegacyNumber(
      src.activeDurationMinMs,
      DEFAULT_SOCIAL_V1_CONFIG.activeDurationMinMs,
    ),
    activeDurationMaxMs: parseLegacyNumber(
      src.activeDurationMaxMs,
      DEFAULT_SOCIAL_V1_CONFIG.activeDurationMaxMs,
    ),
    idleWindowMs: parseLegacyNumber(src.idleWindowMs, DEFAULT_SOCIAL_V1_CONFIG.idleWindowMs),
    idleRetryProbability: parseLegacyNumber(
      src.idleRetryProbability,
      DEFAULT_SOCIAL_V1_CONFIG.idleRetryProbability,
    ),
    idleRetryWaitMs: parseLegacyNumber(src.idleRetryWaitMs, DEFAULT_SOCIAL_V1_CONFIG.idleRetryWaitMs),
    proactiveEnabled: bool(src.proactiveEnabled, DEFAULT_SOCIAL_V1_CONFIG.proactiveEnabled),
    proactiveIdleThresholdMs: parseLegacyNumber(
      src.proactiveIdleThresholdMs,
      DEFAULT_SOCIAL_V1_CONFIG.proactiveIdleThresholdMs,
    ),
    proactiveCheckMinMs: parseLegacyNumber(
      src.proactiveCheckMinMs,
      DEFAULT_SOCIAL_V1_CONFIG.proactiveCheckMinMs,
    ),
    proactiveCheckMaxMs: parseLegacyNumber(
      src.proactiveCheckMaxMs,
      DEFAULT_SOCIAL_V1_CONFIG.proactiveCheckMaxMs,
    ),
    proactiveProbability: parseLegacyNumber(
      src.proactiveProbability,
      DEFAULT_SOCIAL_V1_CONFIG.proactiveProbability,
    ),
    skipProbability: parseLegacyNumber(
      src.skipProbability,
      DEFAULT_SOCIAL_V1_CONFIG.skipProbability,
    ),
    surrenderProbability: parseLegacyNumber(
      src.surrenderProbability,
      DEFAULT_SOCIAL_V1_CONFIG.surrenderProbability,
    ),
    maxReplyChars: parseLegacyNumber(src.maxReplyChars, DEFAULT_SOCIAL_V1_CONFIG.maxReplyChars),
    mustReplyKeywords: Array.isArray(src.mustReplyKeywords)
      ? src.mustReplyKeywords.map((item) => String(item)).filter((item) => item.length > 0)
      : [...DEFAULT_SOCIAL_V1_CONFIG.mustReplyKeywords],
    burstEnabled: bool(src.burstEnabled, DEFAULT_SOCIAL_V1_CONFIG.burstEnabled),
    burstIntervalMinMs: parseLegacyNumber(
      src.burstIntervalMinMs,
      DEFAULT_SOCIAL_V1_CONFIG.burstIntervalMinMs,
    ),
    burstIntervalMaxMs: parseLegacyNumber(
      src.burstIntervalMaxMs,
      DEFAULT_SOCIAL_V1_CONFIG.burstIntervalMaxMs,
    ),
    longGapProbability: parseLegacyNumber(
      src.longGapProbability,
      DEFAULT_SOCIAL_V1_CONFIG.longGapProbability,
    ),
    longGapMinMs: parseLegacyNumber(src.longGapMinMs, DEFAULT_SOCIAL_V1_CONFIG.longGapMinMs),
    longGapMaxMs: parseLegacyNumber(src.longGapMaxMs, DEFAULT_SOCIAL_V1_CONFIG.longGapMaxMs),
  };
}

function buildNumberInputs(cfg: SocialV1Config): Record<NumericSocialKey, string> {
  const out = {} as Record<NumericSocialKey, string>;
  for (const field of RESERVED1_FIELDS) {
    out[field.key] = String(cfg[field.key]);
  }
  return out;
}

function buildBoolDraft(cfg: SocialV1Config): Record<BooleanSocialKey, boolean> {
  return {
    enabled: cfg.enabled,
    activeDurationEnabled: cfg.activeDurationEnabled,
    burstEnabled: cfg.burstEnabled,
    proactiveEnabled: cfg.proactiveEnabled,
  };
}

function phaseMeta(phase: string): { label: string; tone: BadgeTone } {
  switch (phase) {
    case 'active':
      return { label: '活跃 (active)', tone: 'success' };
    case 'probing':
      return { label: '探测中 (probing)', tone: 'accent' };
    case 'exiting':
      return { label: '退场中 (exiting)', tone: 'warn' };
    case 'idle':
      return { label: '空闲 (idle)', tone: 'neutral' };
    default:
      return { label: phase, tone: 'neutral' };
  }
}

function formatEffortOptionLabel(
  option: string,
  labels?: Record<string, DshEffortLabel | string>,
): string {
  const raw = labels?.[option];
  if (typeof raw === 'string' && raw.trim()) {
    return `${option}（${raw.trim()}）`;
  }
  if (raw && typeof raw === 'object') {
    const name = raw.name?.trim();
    const desc = raw.description?.trim();
    if (name && desc) return `${option} — ${name}（${desc}）`;
    if (name) return `${option} — ${name}`;
  }
  return DEFAULT_EFFORT_LABELS[option] ?? option;
}

export const LegacyPage: React.FC = () => {
  const { pushToast, setDirtyEntry, conversations } = useConsole();

  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [actionBusy, setActionBusy] = useState(false);
  const [dshBusy, setDshBusy] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [dshError, setDshError] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<SocialV1Snapshot | null>(null);
  const [boolDraft, setBoolDraft] = useState<Record<BooleanSocialKey, boolean> | null>(null);
  const [numberInputs, setNumberInputs] = useState<Record<NumericSocialKey, string> | null>(null);
  const [keywordsDraft, setKeywordsDraft] = useState<string[]>([]);
  const [newKeyword, setNewKeyword] = useState('');

  const [dshModel, setDshModel] = useState<LegacyAvailability<DshModelInfo> | null>(null);
  const [effort, setEffort] = useState<string>('high');
  const [notifyKey, setNotifyKey] = useState('');
  const [notifyText, setNotifyText] = useState('');

  const loadSeqRef = useRef(0);
  const activeLoadsRef = useRef(0);
  const saveEpochRef = useRef(0);
  const dshGetSeqRef = useRef(0);
  const dshSaveEpochRef = useRef(0);
  const dshSavingRef = useRef(false);
  const effortEditGenRef = useRef(0);
  const effortRef = useRef(effort);
  const dshModelRef = useRef(dshModel);
  const mountedRef = useRef(true);
  const savingRef = useRef(false);
  const snapshotRef = useRef<SocialV1Snapshot | null>(null);
  const boolDraftRef = useRef<Record<BooleanSocialKey, boolean> | null>(null);
  const numberInputsRef = useRef<Record<NumericSocialKey, string> | null>(null);
  const keywordsDraftRef = useRef<string[]>([]);
  const fieldGenRef = useRef<Record<keyof SocialV1Config, number>>(createFieldGenMap());

  snapshotRef.current = snapshot;
  boolDraftRef.current = boolDraft;
  numberInputsRef.current = numberInputs;
  keywordsDraftRef.current = keywordsDraft;
  effortRef.current = effort;
  dshModelRef.current = dshModel;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const bumpFieldGen = useCallback((key: keyof SocialV1Config) => {
    fieldGenRef.current[key] = (fieldGenRef.current[key] ?? 0) + 1;
  }, []);

  const bumpAllFieldGens = useCallback(() => {
    const next = fieldGenRef.current;
    for (const k of Object.keys(next) as (keyof SocialV1Config)[]) {
      next[k] = (next[k] ?? 0) + 1;
    }
  }, []);

  const syncAllDraftsFromConfig = useCallback((cfg: SocialV1Config) => {
    const normalized = normalizeSocialConfig(cfg);
    setBoolDraft(buildBoolDraft(normalized));
    setNumberInputs(buildNumberInputs(normalized));
    setKeywordsDraft([...normalized.mustReplyKeywords]);
  }, []);

  const mergeServerConfigPreservingDirty = useCallback(
    (
      nextCfg: SocialV1Config,
      preserveDirty: boolean,
      startGen: Record<keyof SocialV1Config, number>,
    ) => {
      const prevBase = snapshotRef.current?.config;
      const curBool = boolDraftRef.current;
      const curNums = numberInputsRef.current;
      const curKw = keywordsDraftRef.current;

      if (!prevBase || !curBool || !curNums) {
        syncAllDraftsFromConfig(nextCfg);
        return;
      }

      const nextBool = buildBoolDraft(nextCfg);
      for (const key of BOOLEAN_KEYS) {
        const editedDuringLoad = fieldGenRef.current[key] !== startGen[key];
        const dirtyBeforeLoad = preserveDirty && curBool[key] !== prevBase[key];
        if (editedDuringLoad || dirtyBeforeLoad) {
          nextBool[key] = curBool[key];
        }
      }

      const nextNums = buildNumberInputs(nextCfg);
      for (const field of RESERVED1_FIELDS) {
        const editedDuringLoad = fieldGenRef.current[field.key] !== startGen[field.key];
        const rawText = curNums[field.key].trim();
        const parsed = rawText === '' ? NaN : Number(rawText);
        const dirtyBeforeLoad =
          preserveDirty && (!Number.isFinite(parsed) || parsed !== prevBase[field.key]);
        if (editedDuringLoad || dirtyBeforeLoad) {
          nextNums[field.key] = curNums[field.key];
        }
      }

      const editedKwDuringLoad =
        fieldGenRef.current.mustReplyKeywords !== startGen.mustReplyKeywords;
      const kwDirtyBeforeLoad =
        preserveDirty && JSON.stringify(curKw) !== JSON.stringify(prevBase.mustReplyKeywords);
      setBoolDraft(nextBool);
      setNumberInputs(nextNums);
      setKeywordsDraft(
        editedKwDuringLoad || kwDirtyBeforeLoad ? [...curKw] : [...nextCfg.mustReplyKeywords],
      );
    },
    [syncAllDraftsFromConfig],
  );

  const applyDshModelGetResult = useCallback(
    (
      modelState: LegacyAvailability<DshModelInfo>,
      getSeq: number,
      startSaveEpoch: number,
      startedDuringDshSave: boolean,
      startEffortGen: number,
    ) => {
      if (!mountedRef.current || getSeq !== dshGetSeqRef.current) return;
      if (startedDuringDshSave || startSaveEpoch !== dshSaveEpochRef.current || dshSavingRef.current) {
        return;
      }

      const prevModel = dshModelRef.current;
      const prevServerEffort = prevModel?.available
        ? prevModel.data.reasoningEffort ||
          prevModel.data.providerDefault ||
          prevModel.data.default ||
          ''
        : null;
      const wasDirty = prevServerEffort !== null && effortRef.current !== prevServerEffort;
      const editedDuringGet = effortEditGenRef.current !== startEffortGen;

      setDshModel(modelState);
      setDshError(null);

      if (!modelState.available || wasDirty || editedDuringGet) return;
      const opts =
        Array.isArray(modelState.data.options) && modelState.data.options.length > 0
          ? modelState.data.options
          : [...DEFAULT_EFFORT_OPTIONS];
      const candidate =
        modelState.data.reasoningEffort ||
        modelState.data.providerDefault ||
        modelState.data.default ||
        opts[0];
      if (candidate && opts.includes(candidate)) {
        setEffort(candidate);
      } else if (opts[0]) {
        setEffort(opts[0]);
      }
    },
    [],
  );

  const loadAll = useCallback(
    async (opts?: { preserveDirty?: boolean }) => {
      const seq = ++loadSeqRef.current;
      const dshGetSeq = ++dshGetSeqRef.current;
      const startedDuringSocialSave = savingRef.current;
      const socialEpoch = saveEpochRef.current;
      const startGen = { ...fieldGenRef.current };
      const startedDuringDshSave = dshSavingRef.current;
      const dshSaveEpoch = dshSaveEpochRef.current;
      const startEffortGen = effortEditGenRef.current;

      activeLoadsRef.current += 1;
      setLoading(true);
      setLoadError(null);
      setDshError(null);

      try {
        const [socialResult, modelResult] = await Promise.allSettled([
          fetchSocialV1(),
          fetchDshModel(),
        ]);
        if (!mountedRef.current) return;

        if (seq === loadSeqRef.current) {
          if (socialResult.status === 'fulfilled') {
            const v1 = socialResult.value;
            const normalized = normalizeSocialConfig(v1.config);
            if (!startedDuringSocialSave && socialEpoch === saveEpochRef.current && !savingRef.current) {
              mergeServerConfigPreservingDirty(normalized, Boolean(opts?.preserveDirty), startGen);
              setSnapshot({ ...v1, config: normalized });
            } else {
              setSnapshot((prev) => ({
                config: prev ? prev.config : normalized,
                pendingSummaries: v1.pendingSummaries,
                states: v1.states,
              }));
            }
          } else {
            const err = socialResult.reason;
            setLoadError(err instanceof Error ? err.message : '加载一代仿真配置失败');
          }
        }

        if (modelResult.status === 'fulfilled') {
          applyDshModelGetResult(
            modelResult.value,
            dshGetSeq,
            dshSaveEpoch,
            startedDuringDshSave,
            startEffortGen,
          );
        } else if (
          dshGetSeq === dshGetSeqRef.current &&
          !startedDuringDshSave &&
          dshSaveEpoch === dshSaveEpochRef.current &&
          !dshSavingRef.current
        ) {
          const err = modelResult.reason;
          setDshError(err instanceof Error ? err.message : '读取 DSH 模型状态失败');
        }
      } finally {
        activeLoadsRef.current = Math.max(0, activeLoadsRef.current - 1);
        if (mountedRef.current) {
          setLoading(activeLoadsRef.current > 0);
        }
      }
    },
    [mergeServerConfigPreservingDirty, applyDshModelGetResult],
  );

  useEffect(() => {
    void loadAll({ preserveDirty: false });
  }, [loadAll]);

  const dirtyCount = useMemo(() => {
    if (!snapshot || !boolDraft || !numberInputs) return 0;
    const base = snapshot.config;
    let count = 0;
    for (const key of BOOLEAN_KEYS) {
      if (boolDraft[key] !== base[key]) count += 1;
    }
    for (const field of RESERVED1_FIELDS) {
      const rawText = numberInputs[field.key].trim();
      const parsed = rawText === '' ? NaN : Number(rawText);
      if (!Number.isFinite(parsed) || parsed !== base[field.key]) {
        count += 1;
      }
    }
    if (JSON.stringify(keywordsDraft) !== JSON.stringify(base.mustReplyKeywords)) {
      count += 1;
    }
    return count;
  }, [snapshot, boolDraft, numberInputs, keywordsDraft]);

  const handleSaveConfig = useCallback(async () => {
    if (!snapshot || !boolDraft || !numberInputs || savingRef.current) return;

    const saveStartGen = { ...fieldGenRef.current };
    const payload: Partial<SocialV1Config> = {};

    for (const key of BOOLEAN_KEYS) {
      if (boolDraft[key] !== snapshot.config[key]) {
        payload[key] = boolDraft[key];
      }
    }

    for (const field of RESERVED1_FIELDS) {
      const rawText = numberInputs[field.key].trim();
      const parsed = rawText === '' ? NaN : Number(rawText);
      if (Number.isFinite(parsed) && parsed === snapshot.config[field.key]) {
        continue;
      }
      if (!rawText) {
        pushToast(`${field.label} 不能为空`, 'warn');
        return;
      }
      if (!Number.isFinite(parsed)) {
        pushToast(`${field.label} 必须是有效数字`, 'warn');
        return;
      }
      if (field.integer && !Number.isInteger(parsed)) {
        pushToast(`${field.label} 必须是整数`, 'warn');
        return;
      }
      if (field.min !== undefined && parsed < field.min) {
        pushToast(`${field.label} 不能小于 ${field.min}`, 'warn');
        return;
      }
      if (field.max !== undefined && parsed > field.max) {
        pushToast(`${field.label} 不能大于 ${field.max}`, 'warn');
        return;
      }
      payload[field.key] = parsed;
    }

    if (JSON.stringify(keywordsDraft) !== JSON.stringify(snapshot.config.mustReplyKeywords)) {
      payload.mustReplyKeywords = [...keywordsDraft];
    }

    if (Object.keys(payload).length === 0) {
      return;
    }

    saveEpochRef.current += 1;
    savingRef.current = true;
    setSaving(true);
    try {
      const res = await saveSocialV1(payload);
      if (!mountedRef.current) return;
      saveEpochRef.current += 1;
      const normalized = normalizeSocialConfig(res.config);
      setSnapshot((prev) => (prev ? { ...prev, config: normalized } : prev));

      const latestBool = boolDraftRef.current;
      if (latestBool) {
        const nextBool = buildBoolDraft(normalized);
        for (const key of BOOLEAN_KEYS) {
          if (fieldGenRef.current[key] !== saveStartGen[key]) {
            nextBool[key] = latestBool[key];
          }
        }
        setBoolDraft(nextBool);
      }

      const latestNums = numberInputsRef.current;
      if (latestNums) {
        const nextNums = buildNumberInputs(normalized);
        for (const field of RESERVED1_FIELDS) {
          if (fieldGenRef.current[field.key] !== saveStartGen[field.key]) {
            nextNums[field.key] = latestNums[field.key];
          }
        }
        setNumberInputs(nextNums);
      }

      if (fieldGenRef.current.mustReplyKeywords !== saveStartGen.mustReplyKeywords) {
        setKeywordsDraft([...keywordsDraftRef.current]);
      } else {
        setKeywordsDraft([...normalized.mustReplyKeywords]);
      }

      pushToast('已保存一代仿真参数', 'success');
    } catch (err) {
      if (mountedRef.current) {
        pushToast(err instanceof Error ? err.message : '保存失败', 'danger');
      }
    } finally {
      savingRef.current = false;
      if (mountedRef.current) {
        setSaving(false);
      }
    }
  }, [snapshot, boolDraft, numberInputs, keywordsDraft, pushToast]);

  const handleDiscardConfig = useCallback(() => {
    if (snapshot) {
      bumpAllFieldGens();
      syncAllDraftsFromConfig(snapshot.config);
    }
  }, [snapshot, bumpAllFieldGens, syncAllDraftsFromConfig]);

  useEffect(() => {
    if (dirtyCount > 0) {
      setDirtyEntry({
        count: dirtyCount,
        label: '一代仿真配置有未保存的更改',
        onSave: handleSaveConfig,
        onDiscard: handleDiscardConfig,
      });
    } else {
      setDirtyEntry(null);
    }
    return () => setDirtyEntry(null);
  }, [dirtyCount, handleSaveConfig, handleDiscardConfig, setDirtyEntry]);

  const handleAddKeyword = () => {
    if (!newKeyword.trim()) return;
    if (!keywordsDraft.includes(newKeyword)) {
      bumpFieldGen('mustReplyKeywords');
      setKeywordsDraft([...keywordsDraft, newKeyword]);
    }
    setNewKeyword('');
  };

  const handleFlush = async (key?: string) => {
    if (actionBusy) return;
    setActionBusy(true);
    try {
      await flushSocialV1(key);
      if (!mountedRef.current) return;
      pushToast(key ? `已刷新 ${key} 的一代仿真队列` : '已刷新全部一代仿真队列', 'success');
      await loadAll({ preserveDirty: true });
    } catch (err) {
      if (mountedRef.current) {
        pushToast(err instanceof Error ? err.message : '刷新队列失败', 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setActionBusy(false);
      }
    }
  };

  const handlePhaseChange = async (key: string, phase: 'active' | 'idle') => {
    if (actionBusy) return;
    setActionBusy(true);
    try {
      await setSocialV1Phase(key, phase);
      if (!mountedRef.current) return;
      pushToast(`${key} 阶段已切换为 ${phase === 'active' ? '活跃' : '空闲'}`, 'success');
      await loadAll({ preserveDirty: true });
    } catch (err) {
      if (mountedRef.current) {
        pushToast(err instanceof Error ? err.message : '切换阶段失败', 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setActionBusy(false);
      }
    }
  };

  const handleRefreshDshModel = async () => {
    if (dshBusy) return;
    const myGetSeq = ++dshGetSeqRef.current;
    const startSaveEpoch = dshSaveEpochRef.current;
    const startedDuringDshSave = dshSavingRef.current;
    const startEffortGen = effortEditGenRef.current;
    setDshBusy(true);
    setDshError(null);
    try {
      const res = await fetchDshModel();
      if (!mountedRef.current || myGetSeq !== dshGetSeqRef.current) return;
      applyDshModelGetResult(
        res,
        myGetSeq,
        startSaveEpoch,
        startedDuringDshSave,
        startEffortGen,
      );
      if (res.available) {
        pushToast('已读取 DSH 模型目录信息', 'success');
      } else {
        pushToast('当前隔离运行时已停用 DSH 旧版目录端点', 'info');
      }
    } catch (err) {
      if (
        mountedRef.current &&
        myGetSeq === dshGetSeqRef.current &&
        !startedDuringDshSave &&
        startSaveEpoch === dshSaveEpochRef.current &&
        !dshSavingRef.current
      ) {
        const msg = err instanceof Error ? err.message : '读取 DSH 模型失败';
        setDshError(msg);
        pushToast(msg, 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setDshBusy(false);
      }
    }
  };

  const effortOptions = useMemo(() => {
    if (
      dshModel?.available &&
      Array.isArray(dshModel.data.options) &&
      dshModel.data.options.length > 0
    ) {
      return dshModel.data.options;
    }
    return [...DEFAULT_EFFORT_OPTIONS];
  }, [dshModel]);

  const handleSaveEffort = async () => {
    if (dshBusy) return;
    const selectedEffort = effortOptions.includes(effort) ? effort : (effortOptions[0] ?? effort);
    const startEditGen = effortEditGenRef.current;
    dshSaveEpochRef.current += 1;
    dshSavingRef.current = true;
    setDshBusy(true);
    try {
      const res = await saveDshEffort(selectedEffort);
      if (!mountedRef.current) return;
      if (res.available) {
        const savedEffort = res.data.reasoningEffort || selectedEffort;
        dshSaveEpochRef.current += 1;
        setDshError(null);
        setDshModel((prev) =>
          prev?.available
            ? { available: true, data: { ...prev.data, reasoningEffort: savedEffort } }
            : {
                available: true,
                data: { reasoningEffort: savedEffort, options: effortOptions },
              },
        );
        if (effortEditGenRef.current === startEditGen) {
          setEffort(savedEffort);
        }
        pushToast(`DSH 思考强度已保存为 ${savedEffort}`, 'success');
      } else {
        pushToast(`当前运行时不可用 (${res.status}): ${res.reason}`, 'warn');
      }
    } catch (err) {
      if (mountedRef.current) {
        pushToast(err instanceof Error ? err.message : '保存 DSH 思考强度失败', 'danger');
      }
    } finally {
      dshSavingRef.current = false;
      if (mountedRef.current) {
        setDshBusy(false);
      }
    }
  };

  const handleNotifyDsh = async () => {
    const trimmedKey = notifyKey.trim();
    const trimmedMsg = notifyText.trim();
    if (!/^(group|private):\d+$/.test(trimmedKey)) {
      pushToast('请输入格式合规的会话标识（群聊 group:<群ID> 或单聊 private:<账号>）', 'warn');
      return;
    }
    if (!trimmedMsg) {
      pushToast('请输入要发送给 DSH 的通知内容', 'warn');
      return;
    }
    if (dshBusy) return;
    setDshBusy(true);
    try {
      const res = await notifyDsh(trimmedKey, trimmedMsg);
      if (!mountedRef.current) return;
      if (res.available) {
        pushToast(`已向 ${trimmedKey} 发送 DSH 通知`, 'success');
        setNotifyText((cur) => (cur.trim() === trimmedMsg ? '' : cur));
      } else {
        pushToast(`当前运行时未挂载 DSH 通知接口 (${res.status}): ${res.reason}`, 'warn');
      }
    } catch (err) {
      if (mountedRef.current) {
        pushToast(err instanceof Error ? err.message : '通知 DSH 失败', 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setDshBusy(false);
      }
    }
  };

  const conversationKeys = useMemo(() => {
    const set = new Set<string>();
    if (snapshot) {
      for (const k of Object.keys(snapshot.states || {})) set.add(k);
      for (const k of Object.keys(snapshot.pendingSummaries || {})) set.add(k);
    }
    return Array.from(set).sort();
  }, [snapshot]);

  const notifyKeySuggestions = useMemo(() => {
    const set = new Set<string>(conversationKeys);
    for (const c of conversations) {
      if (c.key) set.add(c.key);
    }
    return Array.from(set).sort();
  }, [conversationKeys, conversations]);

  return (
    <div className="space-y-4">
      <PageHeader
        title="旧版工具隔离区"
        description="集中收纳将在 #78 移除的一代仿真（reserved）与 DSH 专用旧工具，日常运维请使用主导航页。"
        badge={<Badge tone="warn">#78 待退役</Badge>}
        actions={
          <Button
            size="sm"
            variant="secondary"
            disabled={loading}
            onClick={() => void loadAll({ preserveDirty: true })}
          >
            <RefreshCw className="h-3.5 w-3.5" />
            刷新状态
          </Button>
        }
      />

      <Card className="border-amber-300 bg-[var(--warn-soft)]/50 px-4 py-3 dark:border-amber-900">
        <div className="flex items-start gap-2.5 text-xs text-[var(--warn)]">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          <div className="space-y-1">
            <div className="font-semibold">隔离区说明：以下旧版功能计划于 #78 统一退役删除</div>
            <p className="text-[var(--text-secondary)]">
              当前隔离仅用于平滑过渡。新一代控制台已在「总览」「会话」「参与模式与参数」与「模型与用量」中提供原生替代能力；部分仅旧版模式挂载的 DSH 接口在隔离运行时中会返回 503，属于预期行为。
            </p>
          </div>
        </div>
      </Card>

      {loadError && (
        <Card className="px-4 py-3 text-xs text-[var(--danger)]">{loadError}</Card>
      )}

      <Card>
        <CardHeader
          title="一代仿真（reserved）参数与状态"
          subtitle="对应 /api/social* 接口：活跃超时、分条停顿、主动开话题与待摘要队列刷新"
          actions={
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="secondary"
                disabled={actionBusy}
                onClick={() => void handleFlush()}
              >
                刷新全部待处理队列
              </Button>
              <Button
                size="sm"
                variant="primary"
                disabled={saving || dirtyCount === 0}
                onClick={() => void handleSaveConfig()}
              >
                {saving ? '保存中…' : '保存参数'}
              </Button>
            </div>
          }
        />
        {boolDraft && numberInputs ? (
          <div className="space-y-4 p-4">
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
              <label className="flex items-center justify-between rounded-md border border-[var(--border-hairline)] px-3 py-2 text-xs">
                <span>启用一代仿真</span>
                <Switch
                  checked={boolDraft.enabled}
                  onChange={(next) => {
                    bumpFieldGen('enabled');
                    setBoolDraft({ ...boolDraft, enabled: next });
                  }}
                  label="启用一代仿真"
                />
              </label>
              <label className="flex items-center justify-between rounded-md border border-[var(--border-hairline)] px-3 py-2 text-xs">
                <span>启用活跃持续上限</span>
                <Switch
                  checked={boolDraft.activeDurationEnabled}
                  onChange={(next) => {
                    bumpFieldGen('activeDurationEnabled');
                    setBoolDraft({ ...boolDraft, activeDurationEnabled: next });
                  }}
                  label="启用活跃持续上限"
                />
              </label>
              <label className="flex items-center justify-between rounded-md border border-[var(--border-hairline)] px-3 py-2 text-xs">
                <span>启用分条发送（Burst）</span>
                <Switch
                  checked={boolDraft.burstEnabled}
                  onChange={(next) => {
                    bumpFieldGen('burstEnabled');
                    setBoolDraft({ ...boolDraft, burstEnabled: next });
                  }}
                  label="启用分条发送"
                />
              </label>
              <label className="flex items-center justify-between rounded-md border border-[var(--border-hairline)] px-3 py-2 text-xs">
                <span>启用主动开话题</span>
                <Switch
                  checked={boolDraft.proactiveEnabled}
                  onChange={(next) => {
                    bumpFieldGen('proactiveEnabled');
                    setBoolDraft({ ...boolDraft, proactiveEnabled: next });
                  }}
                  label="启用主动开话题"
                />
              </label>
            </div>

            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
              {RESERVED1_FIELDS.map((field) => (
                <label key={field.key} className="space-y-1 text-xs">
                  <span className="block text-[var(--text-secondary)]">{field.label}</span>
                  <input
                    type="number"
                    step={field.step ?? '1'}
                    min={field.min}
                    max={field.max}
                    value={numberInputs[field.key]}
                    onChange={(e) => {
                      bumpFieldGen(field.key);
                      setNumberInputs({ ...numberInputs, [field.key]: e.target.value });
                    }}
                    className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs tabular-nums text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                  />
                </label>
              ))}
            </div>

            <div className="space-y-2 text-xs">
              <span className="block text-[var(--text-secondary)]">
                必回关键词（按完整词条保存，支持包含逗号与空格的精确匹配）
              </span>
              <div className="flex flex-wrap gap-1.5">
                {keywordsDraft.length === 0 ? (
                  <span className="text-[var(--text-muted)]">暂无必回关键词</span>
                ) : (
                  keywordsDraft.map((kw, index) => (
                    <span
                      key={`${kw}-${index}`}
                      className="inline-flex items-center gap-1 rounded border border-[var(--border-hairline)] bg-[var(--bg-subtle)] px-2 py-0.5 font-mono text-[var(--text-primary)]"
                    >
                      <span className="whitespace-pre">{kw}</span>
                      <button
                        type="button"
                        aria-label={`移除关键词 ${kw}`}
                        onClick={() => {
                          bumpFieldGen('mustReplyKeywords');
                          setKeywordsDraft(keywordsDraft.filter((_, idx) => idx !== index));
                        }}
                        className="rounded p-0.5 text-[var(--text-muted)] hover:text-[var(--danger)]"
                      >
                        <X className="h-3 w-3" />
                      </button>
                    </span>
                  ))
                )}
              </div>
              <div className="flex items-center gap-2">
                <input
                  type="text"
                  value={newKeyword}
                  onChange={(e) => setNewKeyword(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      handleAddKeyword();
                    }
                  }}
                  placeholder="输入新关键词后点击添加…"
                  className="h-8 max-w-xs flex-1 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                />
                <Button size="sm" variant="secondary" onClick={handleAddKeyword}>
                  <Plus className="h-3.5 w-3.5" />
                  添加关键词
                </Button>
              </div>
            </div>

            <div className="overflow-x-auto rounded-md border border-[var(--border-hairline)]">
              <table className="w-full border-collapse text-left text-xs">
                <thead className="bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
                  <tr>
                    <th className="px-3 py-2 font-medium">会话</th>
                    <th className="px-3 py-2 font-medium">当前阶段</th>
                    <th className="px-3 py-2 font-medium">待摘要条数</th>
                    <th className="px-3 py-2 font-medium text-right">操作</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-[var(--border-hairline)]">
                  {conversationKeys.length === 0 ? (
                    <tr>
                      <td colSpan={4} className="px-3 py-4 text-center text-[var(--text-muted)]">
                        当前无一代仿真会话状态或待处理摘要
                      </td>
                    </tr>
                  ) : (
                    conversationKeys.map((key) => {
                      const phase = snapshot?.states[key]?.phase || 'idle';
                      const meta = phaseMeta(phase);
                      const pending = snapshot?.pendingSummaries[key] ?? 0;
                      return (
                        <tr key={key}>
                          <td className="px-3 py-2 font-mono">{key}</td>
                          <td className="px-3 py-2">
                            <Badge tone={meta.tone}>{meta.label}</Badge>
                          </td>
                          <td className="px-3 py-2 tabular-nums">{pending}</td>
                          <td className="px-3 py-2 text-right">
                            <div className="inline-flex items-center gap-1.5">
                              {phase !== 'active' && (
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  disabled={actionBusy}
                                  onClick={() => void handlePhaseChange(key, 'active')}
                                >
                                  切换为活跃
                                </Button>
                              )}
                              {phase !== 'idle' && (
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  disabled={actionBusy}
                                  onClick={() => void handlePhaseChange(key, 'idle')}
                                >
                                  切换为空闲
                                </Button>
                              )}
                              <Button
                                size="sm"
                                variant="secondary"
                                disabled={actionBusy}
                                onClick={() => void handleFlush(key)}
                              >
                                刷新队列
                              </Button>
                            </div>
                          </td>
                        </tr>
                      );
                    })
                  )}
                </tbody>
              </table>
            </div>
          </div>
        ) : (
          <div className="p-4 text-xs text-[var(--text-muted)]">
            {loading ? '加载一代仿真状态中…' : '暂无一代仿真配置数据'}
          </div>
        )}
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title={
              <span className="inline-flex items-center gap-1.5">
                <Sparkles className="h-4 w-4 text-[var(--accent)]" />
                DSH 专用工具
              </span>
            }
            subtitle="查看 DSH 模型目录、保存思考强度与发送 DSH 通知"
            actions={
              <Button
                size="sm"
                variant="secondary"
                disabled={dshBusy}
                onClick={() => void handleRefreshDshModel()}
              >
                查看 DSH 模型
              </Button>
            }
          />
          <div className="space-y-4 p-4 text-xs">
            {dshError && (
              <div className="rounded border border-rose-300 bg-[var(--danger-soft)]/50 px-3 py-2 text-[var(--danger)] dark:border-rose-900">
                DSH 模型请求异常：{dshError}
              </div>
            )}

            <div className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/50 p-3">
              {dshModel === null ? (
                <span className="text-[var(--text-muted)]">尚未查询 DSH 模型目录</span>
              ) : dshModel.available ? (
                <dl className="grid grid-cols-2 gap-2">
                  <div>
                    <dt className="text-[var(--text-muted)]">提供方 / 模型</dt>
                    <dd className="font-mono font-medium text-[var(--text-primary)]">
                      {dshModel.data.provider || '—'} / {dshModel.data.model || '—'}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-[var(--text-muted)]">当前思考强度 / 来源</dt>
                    <dd className="font-mono text-[var(--text-primary)]">
                      {dshModel.data.reasoningEffort ||
                        dshModel.data.providerDefault ||
                        dshModel.data.default ||
                        '—'}{' '}
                      ({dshModel.data.catalogSource || 'unknown'})
                    </dd>
                  </div>
                </dl>
              ) : (
                <div className="space-y-1 text-[var(--text-secondary)]">
                  <div className="font-medium text-[var(--text-primary)]">
                    DSH 旧版目录在当前运行时不可用（HTTP {dshModel.status}）
                  </div>
                  <p className="text-[var(--text-muted)]">{dshModel.reason}</p>
                </div>
              )}
            </div>

            <div className="flex flex-wrap items-end gap-2">
              <label className="min-w-[180px] flex-1 space-y-1">
                <span className="block text-[var(--text-secondary)]">思考强度（DSH）</span>
                <select
                  value={effortOptions.includes(effort) ? effort : (effortOptions[0] ?? 'high')}
                  onChange={(e) => {
                    effortEditGenRef.current += 1;
                    setEffort(e.target.value);
                  }}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                >
                  {effortOptions.map((opt) => (
                    <option key={opt} value={opt}>
                      {formatEffortOptionLabel(
                        opt,
                        dshModel?.available ? dshModel.data.labels : undefined,
                      )}
                    </option>
                  ))}
                </select>
              </label>
              <Button
                size="sm"
                variant="primary"
                disabled={dshBusy}
                onClick={() => void handleSaveEffort()}
              >
                保存思考强度
              </Button>
            </div>

            <div className="space-y-2">
              <div className="grid gap-2 sm:grid-cols-[200px_1fr]">
                <label className="space-y-1">
                  <span className="block text-[var(--text-secondary)]">目标会话 key</span>
                  <input
                    type="text"
                    list="legacy-dsh-keys"
                    value={notifyKey}
                    onChange={(e) => setNotifyKey(e.target.value)}
                    placeholder="group:<群号> 或 private:<QQ号>"
                    className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                  />
                  <datalist id="legacy-dsh-keys">
                    {notifyKeySuggestions.map((k) => (
                      <option key={k} value={k} />
                    ))}
                  </datalist>
                </label>
                <label className="space-y-1">
                  <span className="block text-[var(--text-secondary)]">通知 DSH 内容</span>
                  <div className="flex items-center gap-2">
                    <input
                      type="text"
                      value={notifyText}
                      onChange={(e) => setNotifyText(e.target.value)}
                      placeholder="输入要通知 DSH agent 的提醒内容…"
                      className="h-8 flex-1 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
                    />
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={dshBusy}
                      onClick={() => void handleNotifyDsh()}
                    >
                      <Send className="h-3.5 w-3.5" />
                      发送通知
                    </Button>
                  </div>
                </label>
              </div>
            </div>
          </div>
        </Card>

        <Card>
          <CardHeader
            title={
              <span className="inline-flex items-center gap-1.5">
                <Terminal className="h-4 w-4 text-[var(--text-secondary)]" />
                DSH 侧 MCP 工具
              </span>
            }
            subtitle="经本地工具协议挂接给第一代引擎使用（改动后须重启外部宿主以重新拉取清单）；二代接口请看主导航页面。"
          />
          <div className="max-h-80 overflow-y-auto">
            <table className="w-full border-collapse text-left text-xs">
              <thead className="sticky top-0 bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
                <tr>
                  <th className="px-3 py-2 font-medium">工具名</th>
                  <th className="px-3 py-2 font-medium">说明</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--border-hairline)]">
                {DSH_MCP_TOOL_REFERENCE.map((tool) => (
                  <tr key={tool.name}>
                    <td className="px-3 py-2 font-mono text-[var(--text-primary)]">{tool.name}</td>
                    <td className="px-3 py-2 text-[var(--text-secondary)]">{tool.purpose}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Card>
      </div>
    </div>
  );
};
