import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ArrowDown, ArrowUp, KeyRound, RefreshCw, Star } from 'lucide-react';
import { SettingItemView } from '../../api/client';
import {
  DetailedModelRoute,
  DetailedModelsResponse,
  fetchDetailedModelRoutes,
  fetchLegacyModelChain,
  LegacyModelChainResponse,
} from '../../api/config';
import { EmptyState } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader, SettingField } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import {
  buildConversationNameMap,
  formatAppliesLabel,
  formatDateTimeMs,
  formatErrorMessage,
  formatTokenUnit,
} from '../../utils/format';
import {
  ExtraDirtySource,
  INPUT_CLASS,
  MODEL_CREDENTIAL_KEYS,
  MODEL_ENDPOINT_KEY,
  canonicalModelEndpoint,
  localizeSettingItem,
  normalizeEndpointDraftValue,
  resolveActiveModelEndpoint,
  useSettingsDrafts,
} from '../../utils/herHelpers';

const MODEL_SETTING_KEYS = new Set([
  'harness.apiBaseUrl',
  'harness.firstRoute',
  'harness.model',
  'harness.maxSteps',
  'harness.maxTokens',
  'webSearch.model',
  'engine.default',
]);

function isValidSecretValue(value: string): string | null {
  const bytes = new TextEncoder().encode(value);
  if (bytes.length < 8 || bytes.length > 4096) {
    return '密钥长度需在 8 到 4096 字节之间';
  }
  for (let i = 0; i < value.length; i += 1) {
    const code = value.charCodeAt(i);
    if (code < 0x21 || code > 0x7e) {
      return '密钥只能包含可打印 ASCII 字符且不能含空格/空白符';
    }
  }
  return null;
}

export const ModelRoutesTab: React.FC = () => {
  const {
    settings,
    settingsMutationSeq,
    conversations,
    saveSetting,
    resetSetting,
    refreshLiveData,
    setInspector,
    navigate,
    jumpToSetting,
    openConfirm,
    pushToast,
  } = useConsole();

  const [modelsData, setModelsData] = useState<DetailedModelsResponse | null>(null);
  const [modelsSyncedSeq, setModelsSyncedSeq] = useState(0);
  const [legacyChain, setLegacyChain] = useState<LegacyModelChainResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [reordering, setReordering] = useState(false);

  const [secretDrafts, setSecretDrafts] = useState<Record<string, string>>({});
  const [secretBusyKey, setSecretBusyKey] = useState<string | null>(null);

  const mountedRef = useRef(true);
  const loadSeqRef = useRef(0);
  const settingsMutationSeqRef = useRef(settingsMutationSeq);
  settingsMutationSeqRef.current = settingsMutationSeq;
  const lastObservedMutationSeqRef = useRef(settingsMutationSeq);
  const secretDraftsRef = useRef(secretDrafts);
  secretDraftsRef.current = secretDrafts;

  const loadModels = useCallback(
    async (opts?: { syncSettings?: boolean }): Promise<DetailedModelsResponse | null> => {
      const seq = ++loadSeqRef.current;
      const capturedMutationSeq = settingsMutationSeqRef.current;
      setLoading(true);
      try {
        const [modelsRes, legacyRes] = await Promise.allSettled([
          fetchDetailedModelRoutes(),
          fetchLegacyModelChain(),
          ...(opts?.syncSettings ? [refreshLiveData()] : []),
        ]);
        if (!mountedRef.current || seq !== loadSeqRef.current) return null;
        let fetchedModels: DetailedModelsResponse | null = null;
        if (modelsRes.status === 'fulfilled') {
          setModelsData(modelsRes.value);
          setModelsSyncedSeq(capturedMutationSeq);
          fetchedModels = modelsRes.value;
        } else {
          pushToast(formatErrorMessage(modelsRes.reason, '加载模型路由详情失败'), 'danger');
        }
        if (legacyRes.status === 'fulfilled') {
          setLegacyChain(legacyRes.value);
        } else {
          setLegacyChain(null);
        }
        return fetchedModels;
      } finally {
        if (mountedRef.current && seq === loadSeqRef.current) {
          setLoading(false);
        }
      }
    },
    [pushToast, refreshLiveData],
  );

  useEffect(() => {
    mountedRef.current = true;
    void loadModels({ syncSettings: true });
    return () => {
      mountedRef.current = false;
    };
  }, [loadModels]);

  useEffect(() => {
    if (settingsMutationSeq > lastObservedMutationSeqRef.current) {
      lastObservedMutationSeqRef.current = settingsMutationSeq;
      void loadModels();
    }
  }, [settingsMutationSeq, loadModels]);

  const harnessRoutesItem = useMemo(
    () => settings.find((s) => s.key === 'harness.routes') ?? null,
    [settings],
  );
  const firstRouteItem = useMemo(
    () => settings.find((s) => s.key === 'harness.firstRoute') ?? null,
    [settings],
  );

  const curatedModelSettings = useMemo(
    () =>
      settings
        .filter(
          (s) =>
            s.kind.type !== 'password' &&
            s.key !== 'harness.routes' &&
            (s.group === 'model and cost' || MODEL_SETTING_KEYS.has(s.key)),
        )
        .map(localizeSettingItem),
    [settings],
  );

  const secretSettings = useMemo(
    () =>
      settings
        .filter(
          (s) =>
            (s.group === 'endpoint credentials' || s.kind.type === 'password') &&
            s.managedBy !== 'consoleAuth',
        )
        .map(localizeSettingItem),
    [settings],
  );

  const apiBaseUrlItem = useMemo(
    () => settings.find((s) => s.key === 'harness.apiBaseUrl') ?? null,
    [settings],
  );

  const activeEndpointInfo = useMemo(() => {
    const staleModelsEndpoint = modelsSyncedSeq < settingsMutationSeq;
    if (!staleModelsEndpoint && modelsData && 'effectiveModelEndpoint' in modelsData) {
      if (typeof modelsData.effectiveModelEndpoint === 'string' && modelsData.effectiveModelEndpoint) {
        return {
          endpoint:
            canonicalModelEndpoint(modelsData.effectiveModelEndpoint) ??
            modelsData.effectiveModelEndpoint,
          source: modelsData.effectiveModelEndpointSource ?? null,
        };
      }
      return {
        endpoint: null,
        source: modelsData.effectiveModelEndpointSource ?? null,
      };
    }
    if (typeof apiBaseUrlItem?.value === 'string' && apiBaseUrlItem.value) {
      return {
        endpoint: canonicalModelEndpoint(apiBaseUrlItem.value) ?? apiBaseUrlItem.value,
        source: 'settings' as const,
      };
    }
    const itemEndpoint = resolveActiveModelEndpoint(settings);
    if (itemEndpoint) {
      return {
        endpoint: itemEndpoint,
        source: 'dependencies.json' as const,
      };
    }
    return {
      endpoint: null,
      source: null,
    };
  }, [settings, apiBaseUrlItem, modelsData, modelsSyncedSeq, settingsMutationSeq]);

  const dirtySecretKeys = useMemo(
    () => Object.entries(secretDrafts).filter(([, v]) => v.length > 0).map(([k]) => k),
    [secretDrafts],
  );

  const persistValidatedSecret = useCallback(
    async (item: SettingItemView, raw: string, expectedEndpoint?: string): Promise<boolean> => {
      try {
        await saveSetting(item.key, raw, Boolean(item.confirm), expectedEndpoint);
        if (!mountedRef.current) return true;
        setSecretDrafts((prev) => {
          if (prev[item.key] !== raw) return prev;
          const next = { ...prev };
          delete next[item.key];
          return next;
        });
        await loadModels();
        return true;
      } catch (err) {
        if (mountedRef.current) {
          pushToast(formatErrorMessage(err, `保存 ${item.key} 失败`), 'danger');
        }
        return false;
      }
    },
    [saveSetting, pushToast, loadModels],
  );

  const savingRef = useRef(false);

  const prepareSecretSave = useCallback((): ((resolvedEndpoint?: string) => Promise<void>) | null => {
    if (secretBusyKey !== null || savingRef.current) return null;
    const frozenEntries: { item: SettingItemView; raw: string }[] = [];
    for (const key of dirtySecretKeys) {
      const item = secretSettings.find((s) => s.key === key);
      if (!item) continue;
      const raw = secretDraftsRef.current[key] ?? '';
      const validationErr = isValidSecretValue(raw);
      if (validationErr) {
        pushToast(`${key}: ${validationErr}`, 'warn');
        return null;
      }
      frozenEntries.push({ item, raw });
    }
    return async (resolvedEndpoint?: string) => {
      const targetEndpoint = resolvedEndpoint ?? activeEndpointInfo.endpoint ?? undefined;
      for (const { item, raw } of frozenEntries) {
        if (!mountedRef.current) break;
        setSecretBusyKey(item.key);
        let ok = false;
        try {
          ok = await persistValidatedSecret(
            item,
            raw,
            MODEL_CREDENTIAL_KEYS.has(item.key) ? targetEndpoint : undefined,
          );
        } finally {
          if (mountedRef.current) {
            setSecretBusyKey(null);
          }
        }
        if (!ok) break;
      }
    };
  }, [
    secretBusyKey,
    dirtySecretKeys,
    secretSettings,
    pushToast,
    activeEndpointInfo.endpoint,
    persistValidatedSecret,
  ]);

  const saveAllSecrets = useCallback(
    async (resolvedEndpoint?: string) => {
      const runner = prepareSecretSave();
      if (runner) {
        await runner(resolvedEndpoint);
      }
    },
    [prepareSecretSave],
  );

  const discardAllSecrets = useCallback(() => {
    setSecretDrafts({});
  }, []);

  const extraSecretDirty = useMemo<ExtraDirtySource | null>(() => {
    if (dirtySecretKeys.length === 0) return null;
    return {
      count: dirtySecretKeys.length,
      label: `${dirtySecretKeys.length} 项未保存 API 密钥草稿`,
      prepareSave: prepareSecretSave,
      onSave: saveAllSecrets,
      onDiscard: discardAllSecrets,
    };
  }, [dirtySecretKeys.length, prepareSecretSave, saveAllSecrets, discardAllSecrets]);

  const onModelSettingSettled = useCallback(
    async (keys: string[]) => {
      if (
        keys.includes(MODEL_ENDPOINT_KEY) ||
        keys.includes('harness.firstRoute') ||
        keys.includes('harness.routes')
      ) {
        await loadModels();
      }
    },
    [loadModels],
  );

  const { drafts, dirtyKeys, saving, setDraft, clearDraft, handleReset } = useSettingsDrafts(
    curatedModelSettings,
    '模型设置',
    {
      extraDirty: extraSecretDirty,
      onSettled: onModelSettingSettled,
      activeEndpoint: activeEndpointInfo.endpoint,
    },
  );
  savingRef.current = saving;
  const draftsRef = useRef(drafts);
  draftsRef.current = drafts;

  const handleSaveSecret = useCallback(
    async (item: SettingItemView) => {
      if (secretBusyKey !== null || saving) return;
      const raw = secretDraftsRef.current[item.key] ?? '';
      const validationErr = isValidSecretValue(raw);
      if (validationErr) {
        pushToast(`${item.key}: ${validationErr}`, 'warn');
        return;
      }
      const isModelSecret = MODEL_CREDENTIAL_KEYS.has(item.key);
      const shouldSaveEndpoint = isModelSecret && dirtyKeys.includes(MODEL_ENDPOINT_KEY);
      const rawEndpointDraft = shouldSaveEndpoint ? draftsRef.current[MODEL_ENDPOINT_KEY] : undefined;
      const urlToSave = shouldSaveEndpoint
        ? normalizeEndpointDraftValue(
            apiBaseUrlItem ?? { key: MODEL_ENDPOINT_KEY, kind: { type: 'model endpoint' } },
            rawEndpointDraft,
          )
        : undefined;
      const defaultEndpoint = resolveActiveModelEndpoint(settings, null);
      let targetEndpoint = isModelSecret ? activeEndpointInfo.endpoint ?? undefined : undefined;
      setSecretBusyKey(item.key);
      try {
        let endpointSaved = false;
        if (shouldSaveEndpoint) {
          try {
            if (urlToSave === null) {
              await resetSetting(MODEL_ENDPOINT_KEY, false);
            } else {
              await saveSetting(MODEL_ENDPOINT_KEY, urlToSave, false);
            }
            endpointSaved = true;
          } catch (err) {
            if (mountedRef.current) {
              pushToast(
                formatErrorMessage(err, '保存模型接口地址失败，已中止保存密钥'),
                'danger',
              );
            }
            return;
          }
          if (!mountedRef.current) return;
          if (draftsRef.current[MODEL_ENDPOINT_KEY] === rawEndpointDraft) {
            clearDraft(MODEL_ENDPOINT_KEY);
          }
          targetEndpoint =
            typeof urlToSave === 'string'
              ? canonicalModelEndpoint(urlToSave) ?? urlToSave
              : defaultEndpoint;
        }
        try {
          await persistValidatedSecret(item, raw, targetEndpoint);
        } finally {
          if (endpointSaved && mountedRef.current) {
            await loadModels({ syncSettings: true });
          }
        }
      } finally {
        if (mountedRef.current) {
          setSecretBusyKey(null);
        }
      }
    },
    [
      secretBusyKey,
      saving,
      activeEndpointInfo.endpoint,
      dirtyKeys,
      apiBaseUrlItem,
      resetSetting,
      saveSetting,
      clearDraft,
      loadModels,
      settings,
      pushToast,
      persistValidatedSecret,
    ],
  );

  const rawRoutesStored = useMemo(() => {
    if (!harnessRoutesItem || harnessRoutesItem.redacted) return null;
    return Array.isArray(harnessRoutesItem.value)
      ? (harnessRoutesItem.value as Record<string, unknown>[])
      : null;
  }, [harnessRoutesItem]);

  const routesAlignedWithStored = useMemo(() => {
    if (!modelsData || modelsData.error || harnessRoutesItem?.redacted || !rawRoutesStored) {
      return false;
    }
    if (rawRoutesStored.length !== modelsData.routes.length) return false;
    return modelsData.routes.every((route, idx) => {
      const storedItem = rawRoutesStored[idx];
      if (!storedItem || typeof storedItem !== 'object') return false;
      const storedEffort =
        typeof storedItem.reasoningEffort === 'string' ? storedItem.reasoningEffort : null;
      const liveEffort = route.reasoningEffort ?? null;
      const storedMaxOut =
        typeof storedItem.maxOutputTokens === 'number' ? storedItem.maxOutputTokens : null;
      const liveMaxOut = route.maxOutputTokens ?? null;
      return (
        String(storedItem.id ?? '') === route.id &&
        String(storedItem.model ?? '') === route.model &&
        storedEffort === liveEffort &&
        storedMaxOut === liveMaxOut &&
        Boolean(storedItem.retry429) === Boolean(route.retry429)
      );
    });
  }, [modelsData, harnessRoutesItem, rawRoutesStored]);

  const canReorderStoredRoutes = Boolean(
    routesAlignedWithStored && modelsData && modelsData.routes.length > 1,
  );

  const handleMoveRoute = async (index: number, delta: -1 | 1) => {
    if (!canReorderStoredRoutes || !rawRoutesStored || !modelsData || reordering) return;
    const nextIdx = index + delta;
    if (nextIdx < 0 || nextIdx >= rawRoutesStored.length) return;
    const nextArr = [...rawRoutesStored];
    const [moved] = nextArr.splice(index, 1);
    nextArr.splice(nextIdx, 0, moved);
    setReordering(true);
    try {
      await saveSetting('harness.routes', nextArr, false);
      if (!mountedRef.current) return;
      await loadModels();
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '更新路由顺序失败'), 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setReordering(false);
      }
    }
  };

  const handlePinFirstRoute = async (routeId: string) => {
    if (reordering) return;
    setReordering(true);
    try {
      await saveSetting('harness.firstRoute', routeId, false);
      if (!mountedRef.current) return;
      clearDraft('harness.firstRoute');
      await loadModels();
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '设置首选路由失败'), 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setReordering(false);
      }
    }
  };

  const handleClearSecret = (item: SettingItemView) => {
    if (secretBusyKey !== null) return;
    openConfirm({
      title: `确认清空凭据 ${item.key}？`,
      description: '清空后该外部端点凭据将恢复为未设置状态，对应工具可能暂停服务。',
      confirmLabel: '确认清空',
      danger: true,
      onConfirm: async () => {
        setSecretBusyKey(item.key);
        try {
          await resetSetting(item.key, Boolean(item.confirm));
          if (!mountedRef.current) return;
          setSecretDrafts((prev) => {
            const next = { ...prev };
            delete next[item.key];
            return next;
          });
        } catch (err) {
          if (mountedRef.current) {
            pushToast(formatErrorMessage(err, `清空 ${item.key} 失败`), 'danger');
          }
        } finally {
          if (mountedRef.current) {
            setSecretBusyKey(null);
          }
        }
      },
    });
  };

  const healthTone = (state?: string): 'success' | 'danger' | 'neutral' => {
    if (state === 'ok') return 'success';
    if (state === 'failed') return 'danger';
    return 'neutral';
  };

  const overallChainBadge = useMemo((): {
    tone: 'success' | 'warn' | 'danger' | 'neutral';
    label: string;
  } | null => {
    if (!modelsData) return null;
    if (modelsData.error) return { tone: 'danger', label: '路由配置无效' };
    if (modelsData.routes.length === 0) return { tone: 'warn', label: '未配置路由' };
    if (modelsData.allFailed) return { tone: 'danger', label: '全部路由异常' };
    if (modelsData.routes.some((r) => r.health?.state === 'failed')) {
      return { tone: 'warn', label: '部分路由异常' };
    }
    if (modelsData.routes.some((r) => r.health?.state === 'ok')) {
      return { tone: 'success', label: '链路可用' };
    }
    return { tone: 'warn', label: '尚未观测' };
  }, [modelsData]);

  const convNameMap = useMemo(() => buildConversationNameMap(conversations), [conversations]);

  const formatPriceLine = (route: DetailedModelRoute): string => {
    const p = route.price;
    if (!p) return '—';
    const sym = p.symbol || '';
    const unit = formatTokenUnit(p.unit);
    const parts: string[] = [];
    if (p.cacheMiss != null) parts.push(`输入 ${sym}${p.cacheMiss}`);
    if (p.cacheHit != null) parts.push(`命中 ${sym}${p.cacheHit}`);
    if (p.cacheWrite != null) parts.push(`写入 ${sym}${p.cacheWrite}`);
    if (p.output != null) parts.push(`输出 ${sym}${p.output}`);
    const prefix = p.label ? `${p.label} · ` : '';
    return `${prefix}${parts.join(' / ')} (${unit}${p.estimated ? ', 估算' : ''})`;
  };

  const routeErrors = [
    modelsData?.error,
    modelsData?.healthError,
    modelsData?.storeError,
  ].filter((e): e is string => Boolean(e));

  return (
    <div className="space-y-4">
      <Card className="p-4">
        <CardHeader
          title="全局模型路由链（Model route）"
          subtitle="通过设置索引 harness.routes 与 harness.firstRoute 控制路由优先级与实时健康状态"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              {overallChainBadge && (
                <Badge tone={overallChainBadge.tone}>{overallChainBadge.label}</Badge>
              )}
              <Button
                size="sm"
                variant="secondary"
                disabled={loading}
                onClick={() => void loadModels({ syncSettings: true })}
              >
                <RefreshCw className="h-3.5 w-3.5" />
                刷新路由
              </Button>
            </div>
          }
        />

        {routeErrors.length > 0 && (
          <div className="mb-3 space-y-1 rounded-md border border-[var(--warn)]/40 bg-[var(--warn-soft)] px-3 py-2 text-xs text-[var(--text-primary)]">
            {routeErrors.map((errMsg, idx) => (
              <div key={idx}>{errMsg}</div>
            ))}
          </div>
        )}

        {harnessRoutesItem?.redacted && (
          <div className="mb-3 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] px-3 py-2 text-xs text-[var(--text-secondary)]">
            注意：当前 `harness.routes` 已脱敏（redacted），上下移排序已禁用以免覆盖隐藏字段；如需调整首选模型请使用「设为首选」。
          </div>
        )}

        {!harnessRoutesItem?.redacted &&
          rawRoutesStored &&
          modelsData &&
          !modelsData.error &&
          !routesAlignedWithStored && (
            <div className="mb-3 rounded-md border border-[var(--warn)]/40 bg-[var(--warn-soft)] px-3 py-2 text-xs text-[var(--text-primary)]">
              当前设置缓存与实时模型路由列表尚未对齐，请点击「刷新路由」后再执行上下移排序。
            </div>
          )}

        {!modelsData || modelsData.routes.length === 0 ? (
          <EmptyState title={loading ? '加载模型路由中…' : '暂无配置模型路由'} />
        ) : (
          <div className="space-y-2">
            {modelsData.routes.map((route, idx) => {
              const isFirst =
                modelsData.firstRoute === route.id || (!modelsData.firstRoute && idx === 0);
              return (
                <div
                  key={route.id}
                  className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3 text-xs"
                >
                  <div className="flex flex-wrap items-start justify-between gap-2">
                    <div className="space-y-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-mono font-semibold text-[var(--text-primary)]">
                          #{idx + 1} · {route.id}
                        </span>
                        <Badge tone={healthTone(route.health?.state)}>
                          {route.health?.state || 'unknown'}
                        </Badge>
                        {isFirst && <Badge tone="accent">当前首选</Badge>}
                        {route.retry429 && <Badge tone="neutral">429 自动重试</Badge>}
                      </div>
                      <div className="font-mono text-[11px] text-[var(--text-secondary)]">
                        model: {route.model} · effort: {route.reasoningEffort || 'default'} · maxOutputTokens:{' '}
                        {route.maxOutputTokens ?? 'default'}
                      </div>
                      <div className="text-[11px] text-[var(--text-muted)]">
                        费率: {formatPriceLine(route)}
                        {route.health?.class ? ` · 异常类型: ${route.health.class}` : ''}
                        {route.health?.since
                          ? ` · 状态变更时间: ${formatDateTimeMs(route.health.since)}`
                          : ''}
                      </div>
                    </div>

                    <div className="flex flex-wrap items-center gap-1.5">
                      {firstRouteItem && modelsData.firstRoute !== route.id && (
                        <Button
                          size="sm"
                          variant="secondary"
                          disabled={reordering}
                          onClick={() => void handlePinFirstRoute(route.id)}
                        >
                          <Star className="h-3 w-3" />
                          设为首选
                        </Button>
                      )}
                      {canReorderStoredRoutes && (
                        <>
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={reordering || idx === 0}
                            onClick={() => void handleMoveRoute(idx, -1)}
                            aria-label={`上移 ${route.id}`}
                          >
                            <ArrowUp className="h-3.5 w-3.5" />
                          </Button>
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={reordering || idx === modelsData.routes.length - 1}
                            onClick={() => void handleMoveRoute(idx, 1)}
                            aria-label={`下移 ${route.id}`}
                          >
                            <ArrowDown className="h-3.5 w-3.5" />
                          </Button>
                        </>
                      )}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}

        {legacyChain && legacyChain.chain && legacyChain.chain.length > 0 && (
          <details className="mt-3 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/25 px-3 py-2 text-xs">
            <summary className="cursor-pointer font-medium text-[var(--text-secondary)]">
              只读有效链快照（GET /api/model-chain · {legacyChain.chain.length} 项）
            </summary>
            <div className="mt-2 space-y-1 font-mono text-[11px] text-[var(--text-muted)]">
              {legacyChain.chain.map((c, idx) => (
                <div key={`${c.provider}-${c.model}-${idx}`}>
                  #{idx + 1} {c.provider || '—'} / {c.model || '—'} (effort: {c.effort || '—'}, source:{' '}
                  {c.source || '—'})
                </div>
              ))}
            </div>
          </details>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="模型与推理参数"
          subtitle="harness.*、webSearch.model、engine.default 与模型成本相关设置"
          actions={<Badge tone="neutral">{curatedModelSettings.length} 项</Badge>}
        />
        {curatedModelSettings.length === 0 ? (
          <EmptyState title="暂无模型参数设置" />
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
            {curatedModelSettings.map((item) => (
              <div
                key={item.key}
                className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3"
              >
                <div className="mb-2 flex items-center justify-between gap-2">
                  <div className="flex flex-wrap items-center gap-1.5">
                    <Badge tone="neutral">{formatAppliesLabel(item.applies)}</Badge>
                    {item.overriddenBy?.source && (
                      <Badge tone="warn">覆盖: {item.overriddenBy.source}</Badge>
                    )}
                  </div>
                  <button
                    type="button"
                    onClick={() => jumpToSetting(item.key)}
                    className="text-[11px] font-medium text-[var(--accent)] hover:underline"
                  >
                    在设置库中定位
                  </button>
                </div>
                <SettingField
                  setting={item}
                  draftValue={drafts[item.key]}
                  disabled={secretBusyKey !== null || saving}
                  onDraftChange={(next) => setDraft(item.key, next)}
                  onReset={() => handleReset(item.key)}
                />
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="机密设置（端点 API 密钥）"
          subtitle="密钥须为 8–4096 字节可打印 ASCII（不含任何空格）；已保存密钥不在界面回显"
          actions={<KeyRound className="h-4 w-4 text-[var(--text-muted)]" />}
        />
        {activeEndpointInfo.endpoint && (
          <div className="mb-3 flex flex-wrap items-center justify-between gap-2 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/50 px-3 py-2 text-xs">
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-[var(--text-secondary)]">当前生效模型接口地址：</span>
              <span className="font-mono font-medium text-[var(--text-primary)]">
                {activeEndpointInfo.endpoint}
              </span>
            </div>
            <Badge tone={activeEndpointInfo.source === 'settings' ? 'accent' : 'neutral'}>
              来源：
              {activeEndpointInfo.source === 'settings'
                ? '设置库覆盖 (harness.apiBaseUrl)'
                : activeEndpointInfo.source === 'config'
                  ? '配置文件覆盖 (config.json)'
                  : '默认基线 (dependencies.json)'}
            </Badge>
          </div>
        )}
        {secretSettings.length === 0 ? (
          <EmptyState title="当前设置索引中没有可编辑的端点凭据" />
        ) : (
          <div className="space-y-3">
            {secretSettings.map((item) => {
              const draft = secretDrafts[item.key] ?? '';
              const anySecretBusy = secretBusyKey !== null || saving;
              const isFileFallback = item.key === 'harness.apiKey' && !item.isSet && item.source === 'file';
              return (
                <div
                  key={item.key}
                  className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3 text-xs"
                >
                  <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
                    <div>
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-semibold text-[var(--text-primary)]">{item.label}</span>
                        {item.isSet ? (
                          <Badge tone="success">已设置（设置库绑定）</Badge>
                        ) : isFileFallback ? (
                          <Badge tone="accent">已设置（配置文件兜底）</Badge>
                        ) : item.boundElsewhere ? (
                          <Badge tone="warn">端点已变更，需重新输入</Badge>
                        ) : (
                          <Badge tone="warn">未设置</Badge>
                        )}
                        {isFileFallback && item.boundElsewhere && (
                          <Badge tone="neutral">另有其他端点绑定</Badge>
                        )}
                        <Badge tone="neutral">{formatAppliesLabel(item.applies)}</Badge>
                      </div>
                      <div className="mt-0.5 font-mono text-[11px] text-[var(--text-muted)]">
                        {item.key}
                      </div>
                    </div>
                  </div>
                  <div className="flex flex-wrap items-center gap-2">
                    <input
                      type="password"
                      aria-label={item.label}
                      autoComplete="new-password"
                      value={draft}
                      disabled={anySecretBusy}
                      onChange={(e) =>
                        setSecretDrafts((prev) => ({ ...prev, [item.key]: e.target.value }))
                      }
                      placeholder={
                        item.isSet
                          ? '输入新密钥以覆盖旧值'
                          : isFileFallback
                            ? '当前由 config/harness-api-key 兜底，输入新密钥可写入设置库'
                            : item.boundElsewhere
                              ? '端点已变更，请重新输入当前端点密钥（8-4096 ASCII）'
                              : '输入新密钥（8-4096 ASCII）'
                      }
                      className={`${INPUT_CLASS} min-w-[240px] flex-1`}
                    />
                    <Button
                      size="sm"
                      variant="primary"
                      disabled={anySecretBusy || draft.length === 0}
                      onClick={() => void handleSaveSecret(item)}
                    >
                      保存密钥
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={anySecretBusy || (!item.isSet && !item.boundElsewhere)}
                      onClick={() => handleClearSecret(item)}
                    >
                      清空凭据
                    </Button>
                  </div>
                  <p className="mt-1.5 text-[11px] text-[var(--text-muted)]">{item.hint}</p>
                </div>
              );
            })}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="按会话模型路由绑定"
          subtitle="查看各会话覆盖路由及实际解析顺位，可打开检视抽屉或直接跳转至会话页"
          actions={<Badge tone="neutral">{modelsData?.conversations.length ?? 0} 个会话</Badge>}
        />
        {!modelsData || modelsData.conversations.length === 0 ? (
          <EmptyState title="暂无会话路由记录" />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-xs">
              <thead>
                <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                  <th className="py-2 pr-3 font-medium">会话</th>
                  <th className="py-2 pr-3 font-medium">准入</th>
                  <th className="py-2 pr-3 font-medium">存储覆盖</th>
                  <th className="py-2 pr-3 font-medium">生效路由 / 顺位</th>
                  <th className="py-2 pr-3 font-medium">链状态</th>
                  <th className="py-2 text-right font-medium">操作</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--border-hairline)]">
                {modelsData.conversations.map((conv) => {
                  const hasExplicitRoute = Boolean(conv.route);
                  const fallbackOrderFirst = conv.order?.[0] || null;
                  const effectiveRoute =
                    conv.inChain && conv.route ? conv.route : fallbackOrderFirst || conv.route || null;
                  return (
                    <tr key={conv.conversation} className="hover:bg-[var(--bg-subtle)]/50">
                      <td className="py-2 pr-3">
                        <div className="font-medium text-[var(--text-primary)]">
                          {convNameMap.get(conv.conversation) || conv.conversation}
                        </div>
                        {convNameMap.get(conv.conversation) && (
                          <div className="font-mono text-[10.5px] text-[var(--text-muted)]">
                            {conv.conversation}
                          </div>
                        )}
                      </td>
                      <td className="py-2 pr-3">
                        <Badge tone={conv.admitted ? 'success' : 'neutral'}>
                          {conv.admitted ? '已准入' : '未准入'}
                        </Badge>
                      </td>
                      <td className="py-2 pr-3 font-mono text-[11px] text-[var(--text-secondary)]">
                        {conv.stored || '跟随全局'}
                      </td>
                      <td className="py-2 pr-3 font-mono text-[11px] font-semibold text-[var(--text-primary)]">
                        {effectiveRoute
                          ? conv.inChain
                            ? effectiveRoute
                            : `顺位: ${effectiveRoute}`
                          : '—'}
                      </td>
                      <td className="py-2 pr-3">
                        {!hasExplicitRoute ? (
                          <Badge tone="neutral">
                            {fallbackOrderFirst ? '默认顺位' : '未指定'}
                          </Badge>
                        ) : (
                          <Badge tone={conv.inChain ? 'accent' : 'warn'}>
                            {conv.inChain ? '在链内' : '链外（已回落）'}
                          </Badge>
                        )}
                      </td>
                      <td className="py-2 text-right">
                        <div className="inline-flex items-center gap-2.5">
                          <button
                            type="button"
                            onClick={() =>
                              setInspector({ kind: 'conversation', id: conv.conversation })
                            }
                            className="font-medium text-[var(--accent)] hover:underline"
                          >
                            检视
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              navigate(
                                'conversations',
                                undefined,
                                `key=${encodeURIComponent(conv.conversation)}`,
                              );
                            }}
                            className="font-medium text-[var(--text-secondary)] hover:text-[var(--accent)] hover:underline"
                          >
                            前往会话页
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  );
};
