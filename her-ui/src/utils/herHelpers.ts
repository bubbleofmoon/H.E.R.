import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { isSettingWriteLocked, resetSettingValue, saveSettingValue, SettingItemView } from '../api/client';
import { useConsole } from '../stores/consoleStore';
import { formatErrorMessage } from './format';

export const INPUT_CLASS =
  'w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-1.5 text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50';

const SETTING_ZH_OVERRIDES: Record<string, { label: string; hint: string }> = {
  'harness.apiBaseUrl': {
    label: '模型接口地址（harness.apiBaseUrl）',
    hint: 'OpenAI 兼容接口地址，支持回环 HTTP 或 HTTPS（保留显式路径前缀如 /compatible-mode/v1，仅裸域名自动补全 /v1）；留空或恢复默认将回退至 dependencies.json。修改此地址后，原端点绑定的密钥将失效，需重新输入。',
  },
  'harness.model': {
    label: '默认会话模型（harness.model）',
    hint: '未设置 harness.routes 时回合引擎默认使用的中继模型 ID；每回合开始时读取。若两者均未设置则无法启动对话回合。',
  },
  'harness.routes': {
    label: '模型路由链（harness.routes）',
    hint: '按优先级排列的模型路由 JSON 数组（含 id、model 及可选 reasoningEffort、maxOutputTokens、retry429）。恢复默认则回退为单独使用 harness.model。',
  },
  'harness.firstRoute': {
    label: '首选起始路由（harness.firstRoute）',
    hint: '全局首选尝试的模型路由 ID，置顶于默认回退链首；对话级覆盖仍优先。',
  },
  'harness.maxSteps': {
    label: '单回合最大模型请求步数',
    hint: '单个 harness 回合内最多允许调用模型的次数（每次工具调用额外计 1 步）；每回合开始时读取。',
  },
  'harness.maxTokens': {
    label: '单回合 Token 预算上限',
    hint: '单个 harness 回合内所有模型请求累计允许使用的 Token 总预算；每回合开始时读取。',
  },
  'memory.digest.model': {
    label: '记忆摘要模型',
    hint: '记忆摘要任务使用的中继模型 ID；未设置时沿用默认会话模型，每次摘要运行时读取。',
  },
  'memory.digest.timeoutSecs': {
    label: '记忆摘要超时时间（秒）',
    hint: '单次会话记忆摘要遍历模型路由链的总超时秒数上限。',
  },
  'webSearch.model': {
    label: '网页检索辅助模型',
    hint: '用于执行网页联网搜索或摘要的指定模型 ID（留空时沿用默认配置）。',
  },
  'webSearch.apiKey': {
    label: '网页检索 API 密钥',
    hint: '外部网页搜索服务的鉴权密钥（8–4096 字节可打印 ASCII，不含空格）；仅显示已设置状态，不回显原文。',
  },
  'cliModel.apiKey': {
    label: '中继模型服务 API 密钥',
    hint: '与当前生效模型接口地址绑定的 CLI 调用凭据（8–4096 字节可打印 ASCII，不含空格）；仅显示已设置状态，不回显原文。更改模型接口地址后需重新输入。',
  },
  'harness.apiKey': {
    label: '对话模型接口 API 密钥（harness.apiKey）',
    hint: '仅写凭据（8–4096 字节可打印 ASCII，不含空格），与当前生效的模型接口地址绑定；当使用 dependencies.json 默认地址且未在设置库保存时，回退读取 config/harness-api-key。更改模型接口地址后需重新输入。',
  },
  'engine.default': {
    label: '默认回合执行引擎',
    hint: '新会话或未覆写会话默认选用的底层执行引擎标识。',
  },
};

export function localizeSettingItem(item: SettingItemView): SettingItemView {
  const override = SETTING_ZH_OVERRIDES[item.key];
  if (!override) return item;
  return {
    ...item,
    label: override.label,
    hint: override.hint,
  };
}

function containsRedactedMarker(value: unknown): boolean {
  if (typeof value === 'string') {
    return value.includes('[REDACTED]');
  }
  if (Array.isArray(value)) {
    return value.some(containsRedactedMarker);
  }
  if (value && typeof value === 'object') {
    return Object.values(value as Record<string, unknown>).some(containsRedactedMarker);
  }
  return false;
}

export function normalizeEndpointDraftValue(
  item: Pick<SettingItemView, 'key' | 'kind'> | undefined,
  rawValue: unknown,
): unknown {
  if (
    (item?.kind.type === 'model endpoint' || item?.key === MODEL_ENDPOINT_KEY) &&
    typeof rawValue === 'string' &&
    rawValue.trim() === ''
  ) {
    return null;
  }
  return rawValue;
}

export function parseSettingDraftForSave(item: SettingItemView | undefined, rawValue: unknown): unknown {
  if (!item) return rawValue;
  if (item.kind.type === 'model endpoint' || item.key === MODEL_ENDPOINT_KEY) {
    return normalizeEndpointDraftValue(item, rawValue);
  }
  if (item.kind.type === 'route list') {
    let resolved: unknown = rawValue;
    if (typeof rawValue === 'string') {
      const text = rawValue.trim();
      if (!text) {
        throw new Error('请填写完整的路由 JSON 数组（如需清空请使用恢复默认）');
      }
      try {
        resolved = JSON.parse(text);
      } catch {
        throw new Error('路由配置必须是合法的 JSON 数组');
      }
      if (!Array.isArray(resolved)) {
        throw new Error('路由配置顶层必须是 JSON 数组 [...]');
      }
    }
    if (containsRedactedMarker(resolved)) {
      throw new Error('不能提交包含 [REDACTED] 脱敏占位符的路由配置，请填写完整数组');
    }
    return resolved;
  }
  if ((item.kind.type === 'id list' || item.kind.type === 'conversation list') && typeof rawValue === 'string') {
    return rawValue
      .split(/[\s,]+/)
      .map((s) => s.trim())
      .filter(Boolean);
  }
  return rawValue;
}

export interface ExtraDirtySource {
  count: number;
  label: string;
  prepareSave?: () => ((resolvedEndpoint?: string) => Promise<void>) | null;
  onSave: (resolvedEndpoint?: string) => Promise<void>;
  onDiscard: () => void;
}

export const MODEL_ENDPOINT_KEY = 'harness.apiBaseUrl';
export const MODEL_CREDENTIAL_KEYS = new Set(['harness.apiKey', 'cliModel.apiKey']);

export function canonicalModelEndpoint(url: unknown): string | undefined {
  if (typeof url !== 'string') return undefined;
  const trimmed = url.replace(/\/+$/, '');
  return trimmed.length > 0 ? trimmed : undefined;
}

export function resolveActiveModelEndpoint(
  items: SettingItemView[],
  overrideEndpoint?: unknown,
): string | undefined {
  const endpointItem = items.find((s) => s.key === MODEL_ENDPOINT_KEY);
  const fallback = canonicalModelEndpoint(endpointItem?.fallback?.value);
  if (overrideEndpoint !== undefined) {
    return canonicalModelEndpoint(overrideEndpoint) ?? fallback;
  }
  return canonicalModelEndpoint(endpointItem?.value) ?? fallback;
}

export function utf16Length(value: string): number {
  return value.length;
}

export function isSingleLineWithin(value: string, maxUnits: number): boolean {
  return !/[\u0000-\u001F\u007F\u2028\u2029]/.test(value) && utf16Length(value) <= maxUnits;
}

export function unicodeScalarLength(value: string): number {
  return Array.from(value).length;
}

export function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

function normalizeForSettingCompare(value: unknown): unknown {
  if (value === undefined || value === null || value === '') {
    return null;
  }
  if (Array.isArray(value)) {
    return value.map(normalizeForSettingCompare);
  }
  if (typeof value === 'object') {
    const obj = value as Record<string, unknown>;
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(obj).sort()) {
      const normalized = normalizeForSettingCompare(obj[key]);
      if (normalized !== null) {
        sorted[key] = normalized;
      }
    }
    return sorted;
  }
  return value;
}

export function settingValuesEqual(a: unknown, b: unknown): boolean {
  return (
    JSON.stringify(normalizeForSettingCompare(a)) ===
    JSON.stringify(normalizeForSettingCompare(b))
  );
}

export function useSettingsDrafts(
  items: SettingItemView[],
  labelPrefix: string,
  options: {
    manageDirtyBar?: boolean;
    extraDirty?: ExtraDirtySource | null;
    onSettled?: (keys: string[]) => void | Promise<void>;
  } = {},
) {
  const { resetSetting, openConfirm, pushToast, refreshLiveData, setDirtyEntry } = useConsole();
  const [drafts, setDrafts] = useState<Record<string, unknown>>({});
  const [saving, setSaving] = useState(false);
  const mountedRef = useRef(true);
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const draftRevRef = useRef<Record<string, number>>({});
  const manageDirtyBar = options.manageDirtyBar !== false;
  const extraDirty = options.extraDirty ?? null;
  const onSettledRef = useRef(options.onSettled);
  onSettledRef.current = options.onSettled;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (manageDirtyBar) {
        setDirtyEntry(null);
      }
    };
  }, [manageDirtyBar, setDirtyEntry]);

  const dirtyKeys = useMemo(
    () =>
      Object.entries(drafts)
        .filter(([key, val]) => {
          const item = items.find((s) => s.key === key);
          if (!item) return false;
          if (item.redacted && item.kind.type === 'route list') {
            return typeof val === 'string' ? val.trim().length > 0 : true;
          }
          if (
            (item.kind.type === 'id list' || item.kind.type === 'conversation list') &&
            typeof val === 'string'
          ) {
            return !settingValuesEqual(parseSettingDraftForSave(item, val), item.value);
          }
          return !settingValuesEqual(val, item.value);
        })
        .map(([key]) => key),
    [drafts, items],
  );

  const setDraft = useCallback((key: string, value: unknown) => {
    draftRevRef.current[key] = (draftRevRef.current[key] ?? 0) + 1;
    setDrafts((prev) => ({ ...prev, [key]: value }));
  }, []);

  const clearDraft = useCallback((key: string) => {
    draftRevRef.current[key] = (draftRevRef.current[key] ?? 0) + 1;
    setDrafts((prev) => {
      if (!(key in prev)) return prev;
      const next = { ...prev };
      delete next[key];
      return next;
    });
  }, []);

  const discardAll = useCallback(() => {
    for (const key of Object.keys(draftRevRef.current)) {
      draftRevRef.current[key] = (draftRevRef.current[key] ?? 0) + 1;
    }
    setDrafts({});
  }, []);

  const executeSaveBatch = useCallback(
    async (
      snapshots: {
        key: string;
        value: unknown;
        rev: number;
        confirm: boolean;
        item?: SettingItemView;
      }[],
    ): Promise<{ saved: number; failed: string[]; resolvedEndpoint?: string }> => {
      if (snapshots.length === 0) {
        return { saved: 0, failed: [] };
      }

      const active = snapshots.filter(({ key, rev }) => (draftRevRef.current[key] ?? 0) === rev);
      if (active.length === 0) {
        return { saved: 0, failed: [] };
      }

      const saveOne = async ({
        key,
        value,
        rev,
        confirm,
        item,
        expectedEndpoint,
      }: {
        key: string;
        value: unknown;
        rev: number;
        confirm: boolean;
        item?: SettingItemView;
        expectedEndpoint?: string;
      }) => {
        try {
          const parsedValue = parseSettingDraftForSave(item, value);
          const isClearedEndpoint =
            (item?.kind.type === 'model endpoint' || key === MODEL_ENDPOINT_KEY) &&
            parsedValue === null;
          const res = isClearedEndpoint
            ? await resetSettingValue(key, confirm)
            : await saveSettingValue(key, parsedValue, confirm, expectedEndpoint);
          const mirrorErr =
            typeof res.mirrorError === 'string' && res.mirrorError ? res.mirrorError : null;
          const wakeErr =
            typeof res.wakeRefreshError === 'string' && res.wakeRefreshError
              ? res.wakeRefreshError
              : null;
          const warn = [mirrorErr, wakeErr].filter(Boolean).join('；') || null;
          return { key, rev, ok: true as const, warn, value: res.value };
        } catch (err) {
          return {
            key,
            rev,
            ok: false as const,
            error: `${key}: ${formatErrorMessage(err, '保存失败')}`,
          };
        }
      };

      const endpointEntry = active.find((entry) => entry.key === MODEL_ENDPOINT_KEY);
      const otherEntries = active.filter((entry) => entry.key !== MODEL_ENDPOINT_KEY);
      const outcomes: Awaited<ReturnType<typeof saveOne>>[] = [];
      let resolvedEndpoint = resolveActiveModelEndpoint(itemsRef.current);

      if (endpointEntry) {
        const epOutcome = await saveOne(endpointEntry);
        outcomes.push(epOutcome);
        if (!epOutcome.ok) {
          pushToast(`部分${labelPrefix}保存失败：${epOutcome.error}`, 'danger');
          return { saved: 0, failed: [epOutcome.error], resolvedEndpoint };
        }
        resolvedEndpoint = resolveActiveModelEndpoint(itemsRef.current, epOutcome.value);
      }

      // Reserve every remaining key's per-key write lock in api/client.ts synchronously on tick 0.
      const restOutcomes = await Promise.all(
        otherEntries.map((entry) =>
          saveOne({
            ...entry,
            expectedEndpoint: MODEL_CREDENTIAL_KEYS.has(entry.key) ? resolvedEndpoint : undefined,
          }),
        ),
      );
      outcomes.push(...restOutcomes);

      const failed: string[] = [];
      const warnings: string[] = [];
      const savedEntries: { key: string; rev: number }[] = [];

      for (const out of outcomes) {
        if (out.ok) {
          savedEntries.push({ key: out.key, rev: out.rev });
          if (out.warn) warnings.push(`${out.key}: ${out.warn}`);
        } else {
          failed.push(out.error);
        }
      }

      if (savedEntries.length > 0) {
        await refreshLiveData();
        await onSettledRef.current?.(savedEntries.map((e) => e.key));
        if (mountedRef.current) {
          setDrafts((prev) => {
            let changed = false;
            const next = { ...prev };
            for (const { key, rev } of savedEntries) {
              if (key in next && (draftRevRef.current[key] ?? 0) === rev) {
                delete next[key];
                changed = true;
              }
            }
            return changed ? next : prev;
          });
        }
      }

      if (failed.length > 0) {
        pushToast(`部分${labelPrefix}保存失败：${failed[0]}`, 'danger');
      } else if (warnings.length > 0) {
        pushToast(
          `已保存 ${savedEntries.length} 项${labelPrefix}（写回警告：${warnings.join('；')}）`,
          'warn',
        );
      } else if (savedEntries.length > 0) {
        pushToast(`已保存 ${savedEntries.length} 项${labelPrefix}`, 'success');
      }

      return { saved: savedEntries.length, failed, resolvedEndpoint };
    },
    [refreshLiveData, pushToast, labelPrefix],
  );

  const saveAll = useCallback(
    async (
      afterExtraSave?: (resolvedEndpoint?: string) => Promise<void>,
    ): Promise<{
      saved: number;
      failed: string[];
      deferred?: boolean;
      resolvedEndpoint?: string;
    }> => {
      if (dirtyKeys.length === 0) {
        return { saved: 0, failed: [] };
      }
      const snapshots = dirtyKeys.map((key) => {
        const item = itemsRef.current.find((s) => s.key === key);
        return {
          key,
          value: drafts[key],
          rev: draftRevRef.current[key] ?? 0,
          confirm: Boolean(item?.confirm),
          item,
        };
      });
      const confirmKeys = snapshots.filter((s) => s.confirm).map((s) => s.key);
      if (confirmKeys.length > 0) {
        openConfirm({
          title: `确认修改 ${confirmKeys.length} 项安全设置？`,
          description: `待保存的受保护设置：${confirmKeys.join('、')}。\n确认后将立即生效，并在必要时重新评估访问权限。`,
          confirmLabel: '确认保存',
          danger: true,
          onConfirm: async () => {
            if (mountedRef.current) setSaving(true);
            try {
              const batch = await executeSaveBatch(snapshots);
              if (batch.failed.length === 0 && afterExtraSave) {
                await afterExtraSave(batch.resolvedEndpoint);
              }
            } finally {
              if (mountedRef.current) setSaving(false);
            }
          },
        });
        return { saved: 0, failed: [], deferred: true };
      }
      return executeSaveBatch(snapshots);
    },
    [dirtyKeys, drafts, executeSaveBatch, openConfirm],
  );

  useEffect(() => {
    if (!manageDirtyBar) return;
    const extraCount = extraDirty?.count ?? 0;
    const totalCount = dirtyKeys.length + extraCount;
    if (totalCount > 0) {
      const label =
        dirtyKeys.length > 0 && extraCount > 0
          ? `${labelPrefix}有 ${dirtyKeys.length} 项设置与 ${extraDirty?.label}`
          : dirtyKeys.length > 0
            ? `${labelPrefix}有 ${dirtyKeys.length} 项未保存更改`
            : extraDirty?.label || `有 ${totalCount} 项未保存更改`;
      setDirtyEntry({
        count: totalCount,
        label,
        onSave: async () => {
          const runExtra = extraDirty?.prepareSave
            ? extraDirty.prepareSave()
            : extraDirty && extraDirty.count > 0
              ? extraDirty.onSave
              : undefined;
          if (extraDirty?.prepareSave && !runExtra) {
            return;
          }
          if (mountedRef.current) setSaving(true);
          try {
            let resolvedEndpoint: string | undefined;
            if (dirtyKeys.length > 0) {
              const outcome = await saveAll(runExtra ?? undefined);
              if (outcome.deferred || outcome.failed.length > 0) {
                return;
              }
              resolvedEndpoint = outcome.resolvedEndpoint;
            }
            if (runExtra) {
              await runExtra(resolvedEndpoint);
            }
          } finally {
            if (mountedRef.current) setSaving(false);
          }
        },
        onDiscard: () => {
          discardAll();
          extraDirty?.onDiscard();
        },
      });
    } else {
      setDirtyEntry(null);
    }
  }, [manageDirtyBar, dirtyKeys.length, labelPrefix, extraDirty, setDirtyEntry, saveAll, discardAll]);

  const handleReset = useCallback(
    (key: string) => {
      const item = itemsRef.current.find((s) => s.key === key);
      const needsConfirm = Boolean(item?.confirm);
      openConfirm({
        title: needsConfirm ? `确认恢复安全设置 ${key} 为默认值？` : `确认恢复 ${key} 为默认值？`,
        description: needsConfirm
          ? '此操作会重置受保护的安全设置并移除未保存草稿，在必要时重新评估访问权限。'
          : '恢复后将清除该设置项的自定义值，并同步移除未保存草稿。',
        confirmLabel: '确认恢复',
        danger: needsConfirm,
        onConfirm: async () => {
          if (isSettingWriteLocked(key)) {
            pushToast(`${key} 正在写入中，请稍后再恢复默认`, 'warn');
            return;
          }
          const rev = draftRevRef.current[key] ?? 0;
          if (mountedRef.current) setSaving(true);
          try {
            await resetSetting(key, needsConfirm);
            await onSettledRef.current?.([key]);
            if (!mountedRef.current) return;
            setDrafts((prev) => {
              if (!(key in prev) || (draftRevRef.current[key] ?? 0) !== rev) {
                return prev;
              }
              const next = { ...prev };
              delete next[key];
              return next;
            });
          } catch (err) {
            pushToast(formatErrorMessage(err, `恢复 ${key} 默认值失败`), 'danger');
          } finally {
            if (mountedRef.current) setSaving(false);
          }
        },
      });
    },
    [openConfirm, resetSetting, pushToast],
  );

  return {
    drafts,
    dirtyKeys,
    saving,
    setDraft,
    clearDraft,
    discardAll,
    saveAll,
    handleReset,
  };
}

export function useGuardedTab<T extends string>(
  pageId: string,
  defaultTab: T,
  validTabs: readonly T[],
): [T, (nextTab: string) => void] {
  const { activeTabs, setActiveTab, dirtyEntry, openConfirm } = useConsole();
  const rawTab = activeTabs[pageId];
  const currentTab = validTabs.includes(rawTab as T) ? (rawTab as T) : defaultTab;

  const handleTabChange = useCallback(
    (nextTab: string) => {
      if (nextTab === currentTab || !validTabs.includes(nextTab as T)) return;
      if (dirtyEntry && dirtyEntry.count > 0) {
        openConfirm({
          title: '丢弃未保存的更改？',
          description: `${dirtyEntry.label || '当前标签页有未保存的更改'}，切换标签页将会丢失这些修改。`,
          confirmLabel: '放弃并切换',
          danger: true,
          onConfirm: () => {
            dirtyEntry.onDiscard();
            setActiveTab(pageId, nextTab);
          },
        });
        return;
      }
      setActiveTab(pageId, nextTab);
    },
    [currentTab, validTabs, dirtyEntry, openConfirm, setActiveTab, pageId],
  );

  return [currentTab, handleTabChange];
}
