import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, RefreshCw } from 'lucide-react';
import {
  fetchTokenRecent,
  fetchTokensByProvider,
  fetchTokenSeries,
  fetchTokenSummary,
  fetchTokenTurns,
  fetchUsageAttribution,
  resetTokenLedger,
  TokenRecentEntry,
  TokensByProviderResponse,
  TokenSeriesResponse,
  TokenSummaryResponse,
  TokenTurnRow,
  UsageAttributionResponse,
} from '../../api/config';
import { EmptyState, KeyValueRow, SegmentedControl } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader, Switch } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import {
  buildConversationNameMap,
  formatDateTimeMs,
  formatErrorMessage,
  formatTokenUnit,
} from '../../utils/format';
import { SpendChart } from './SpendChart';

type WindowOption = '24h' | '3d' | '7d' | '30d';

const WINDOW_PARAMS: Record<WindowOption, { hours: 24 | 72 | 168 | 720; days: number }> = {
  '24h': { hours: 24, days: 1 },
  '3d': { hours: 72, days: 3 },
  '7d': { hours: 168, days: 7 },
  '30d': { hours: 720, days: 30 },
};

const WEEKDAY_NAMES: Record<number, string> = {
  1: '周一',
  2: '周二',
  3: '周三',
  4: '周四',
  5: '周五',
  6: '周六',
  7: '周日',
};

function formatPeakWindows(windows?: [number, number][]): string {
  if (!Array.isArray(windows) || windows.length === 0) return '—';
  return windows.map(([start, end]) => `${start}:00–${end}:00`).join(', ');
}

function formatPeakWeekdays(days?: number[]): string {
  if (!Array.isArray(days) || days.length === 0) return '—';
  return days.map((d) => WEEKDAY_NAMES[d] || String(d)).join(' / ');
}

export const SpendTab: React.FC = () => {
  const { conversations, setInspector, openConfirm, pushToast } = useConsole();

  const [windowOpt, setWindowOpt] = useState<WindowOption>('24h');
  const [seriesWindow, setSeriesWindow] = useState<WindowOption>('24h');
  const [attributionWindow, setAttributionWindow] = useState<WindowOption>('24h');
  const [metric, setMetric] = useState<'cost' | 'tokens'>('cost');
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [loading, setLoading] = useState(true);
  const [partialError, setPartialError] = useState<string | null>(null);

  const [series, setSeries] = useState<TokenSeriesResponse | null>(null);
  const [summary, setSummary] = useState<TokenSummaryResponse | null>(null);
  const [byProvider, setByProvider] = useState<TokensByProviderResponse | null>(null);
  const [attribution, setAttribution] = useState<UsageAttributionResponse | null>(null);
  const [recent, setRecent] = useState<TokenRecentEntry[]>([]);

  const [selectedConvKey, setSelectedConvKey] = useState<string | null>(null);
  const [turns, setTurns] = useState<TokenTurnRow[]>([]);
  const [turnsError, setTurnsError] = useState<string | null>(null);
  const [loadingTurns, setLoadingTurns] = useState(false);

  const mountedRef = useRef(true);
  const loadSeqRef = useRef(0);
  const turnsSeqRef = useRef(0);

  const loadAll = useCallback(
    async (win: WindowOption, opts?: { silent?: boolean }) => {
      const seq = ++loadSeqRef.current;
      const { hours, days } = WINDOW_PARAMS[win];
      if (!opts?.silent) {
        setLoading(true);
      }
      try {
        const [seriesRes, summaryRes, providerRes, attrRes, recentRes] =
          await Promise.allSettled([
            fetchTokenSeries(hours, 48),
            fetchTokenSummary(200),
            fetchTokensByProvider(),
            fetchUsageAttribution(days),
            fetchTokenRecent(50),
          ]);
        if (!mountedRef.current || seq !== loadSeqRef.current) return;

        const failedParts: string[] = [];
        if (seriesRes.status === 'fulfilled') {
          setSeries(seriesRes.value);
          setSeriesWindow(win);
        } else {
          failedParts.push(`时间序列: ${formatErrorMessage(seriesRes.reason)}`);
        }
        if (summaryRes.status === 'fulfilled') {
          setSummary(summaryRes.value);
        } else {
          failedParts.push(`账本汇总: ${formatErrorMessage(summaryRes.reason)}`);
        }
        if (providerRes.status === 'fulfilled') {
          setByProvider(providerRes.value);
        } else {
          failedParts.push(`提供商分布: ${formatErrorMessage(providerRes.reason)}`);
        }
        if (attrRes.status === 'fulfilled') {
          setAttribution(attrRes.value);
          setAttributionWindow(win);
        } else {
          failedParts.push(`归因统计: ${formatErrorMessage(attrRes.reason)}`);
        }
        if (recentRes.status === 'fulfilled') {
          setRecent(Array.isArray(recentRes.value.entries) ? recentRes.value.entries : []);
        } else {
          failedParts.push(`最近流水: ${formatErrorMessage(recentRes.reason)}`);
        }

        if (failedParts.length > 0) {
          const msg = failedParts[0];
          setPartialError(`部分统计加载失败（${failedParts.join('；')}）`);
          if (!opts?.silent) {
            pushToast(msg, 'danger');
          }
        } else {
          setPartialError(null);
        }
      } finally {
        if (mountedRef.current && seq === loadSeqRef.current) {
          setLoading(false);
        }
      }
    },
    [pushToast],
  );

  const loadConvTurns = useCallback(
    async (convKey: string) => {
      const seq = ++turnsSeqRef.current;
      setSelectedConvKey(convKey);
      setTurns([]);
      setTurnsError(null);
      setLoadingTurns(true);
      try {
        const res = await fetchTokenTurns(convKey, 200);
        if (!mountedRef.current || seq !== turnsSeqRef.current) return;
        setTurns(Array.isArray(res.turns) ? res.turns : []);
      } catch (err) {
        if (mountedRef.current && seq === turnsSeqRef.current) {
          const msg = formatErrorMessage(err, `加载会话 ${convKey} 回合花费失败`);
          setTurnsError(msg);
          pushToast(msg, 'danger');
        }
      } finally {
        if (mountedRef.current && seq === turnsSeqRef.current) {
          setLoadingTurns(false);
        }
      }
    },
    [pushToast],
  );

  useEffect(() => {
    mountedRef.current = true;
    void loadAll(windowOpt);
    return () => {
      mountedRef.current = false;
    };
  }, [loadAll, windowOpt]);

  useEffect(() => {
    if (!autoRefresh) return undefined;
    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') {
        void loadAll(windowOpt, { silent: true });
      }
    }, 15_000);
    return () => window.clearInterval(timer);
  }, [autoRefresh, loadAll, windowOpt]);

  const symbol = summary?.symbol || series?.symbol || attribution?.symbol || '¥';

  const convNameMap = useMemo(() => buildConversationNameMap(conversations), [conversations]);

  const handleResetLedger = () => {
    openConfirm({
      title: '确认清空全部 Token 与花费账本？',
      description:
        '此操作将调用 POST /api/tokens/reset 重置用量账本。若当前运行于选定运行时模式，后端路由策略会拒绝该请求（HTTP 503）。',
      confirmLabel: '确认清空账本',
      danger: true,
      onConfirm: async () => {
        try {
          await resetTokenLedger();
          if (!mountedRef.current) return;
          pushToast('已重置用量账本', 'success');
          setSelectedConvKey(null);
          setTurns([]);
          setTurnsError(null);
          await loadAll(windowOpt);
        } catch (err) {
          if (mountedRef.current) {
            pushToast(formatErrorMessage(err, '重置用量账本失败'), 'danger');
          }
        }
      },
    });
  };

  const priceModels = useMemo(() => {
    const raw = summary?.priceTable?.models;
    if (!raw || typeof raw !== 'object') return [];
    return Object.entries(raw as Record<string, unknown>);
  }, [summary]);

  const tierLabel = summary?.nowTier?.tierLabel || summary?.nowTier?.tier || '标准档';
  const windowMismatch = seriesWindow !== windowOpt || attributionWindow !== windowOpt;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <SegmentedControl
          value={windowOpt}
          onChange={setWindowOpt}
          options={[
            { value: '24h', label: '24 小时' },
            { value: '3d', label: '3 天' },
            { value: '7d', label: '7 天' },
            { value: '30d', label: '30 天' },
          ]}
        />

        <div className="flex flex-wrap items-center gap-3">
          <div className="flex items-center gap-2 text-xs text-[var(--text-secondary)]">
            <Switch
              checked={autoRefresh}
              onChange={setAutoRefresh}
              label="15 秒自动刷新"
            />
            <span>15s 自动刷新</span>
          </div>
          <Button
            size="sm"
            variant="secondary"
            disabled={loading}
            onClick={() => void loadAll(windowOpt)}
          >
            <RefreshCw className="h-3.5 w-3.5" />
            刷新数据
          </Button>
        </div>
      </div>

      {partialError && (
        <div className="rounded-md border border-[var(--warn)]/40 bg-[var(--warn-soft)] px-3 py-2 text-xs text-[var(--text-primary)]">
          {partialError}
          {windowMismatch
            ? `（当前时间序列展示 ${seriesWindow}、归因对照展示 ${attributionWindow}）`
            : ''}
        </div>
      )}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Card className="p-3.5">
          <div className="text-[11px] text-[var(--text-muted)]">累计总花费（摘要账本）</div>
          <div className="mt-1 text-lg font-semibold text-[var(--text-primary)]">
            {symbol}
            {(summary?.totals.cost ?? 0).toFixed(4)}
          </div>
          <div className="mt-1 text-[11px] text-[var(--text-secondary)]">
            已归因 {symbol}
            {(summary?.totals.attributedCost ?? 0).toFixed(4)} · 未归因 {symbol}
            {(summary?.totals.unattributedCost ?? 0).toFixed(4)}
          </div>
        </Card>

        <Card className="p-3.5">
          <div className="text-[11px] text-[var(--text-muted)]">
            窗口内花费（{seriesWindow}）
          </div>
          <div className="mt-1 text-lg font-semibold text-[var(--text-primary)]">
            {symbol}
            {(series?.totals.cost ?? 0).toFixed(4)}
          </div>
          <div className="mt-1 text-[11px] text-[var(--text-secondary)]">
            {(series?.totals.tokens ?? 0).toLocaleString()} tokens · {series?.totals.turns ?? 0} 回合
          </div>
        </Card>

        <Card className="p-3.5">
          <div className="text-[11px] text-[var(--text-muted)]">缓存命中与平均成本</div>
          <div className="mt-1 text-lg font-semibold text-[var(--text-primary)]">
            {summary?.totals.cacheHitRate != null
              ? `${(summary.totals.cacheHitRate * 100).toFixed(1)}%`
              : '—'}
          </div>
          <div className="mt-1 text-[11px] text-[var(--text-secondary)]">
            均值/回合{' '}
            {summary?.totals.avgCostPerTurn != null
              ? `${symbol}${summary.totals.avgCostPerTurn.toFixed(4)}`
              : '—'}
          </div>
        </Card>

        <Card className="p-3.5">
          <div className="text-[11px] text-[var(--text-muted)]">当前计费档位与规模</div>
          <div className="mt-1 flex items-center gap-2">
            <Badge tone="accent">{tierLabel}</Badge>
            {(summary?.nowTier?.estimated || summary?.totals.estimated) && (
              <Badge tone="warn">含估算</Badge>
            )}
          </div>
          <div className="mt-1.5 text-[11px] text-[var(--text-secondary)]">
            活跃会话 {summary?.totals.conversations ?? 0} · 步数 {summary?.totals.steps ?? 0}
          </div>
        </Card>
      </div>

      <Card className="p-4">
        <CardHeader
          title={`时间序列趋势（${seriesWindow}）`}
          subtitle={
            series?.offChartCost || series?.offChartTokens
              ? `另有窗口外历史样本：${symbol}${(series.offChartCost ?? 0).toFixed(4)} / ${(series.offChartTokens ?? 0).toLocaleString()} tokens`
              : `有效采样 ${series?.countedSamples ?? 0} 条`
          }
          actions={
            <SegmentedControl
              value={metric}
              onChange={setMetric}
              options={[
                { value: 'cost', label: '按花费' },
                { value: 'tokens', label: '按 Tokens' },
              ]}
            />
          }
        />
        <SpendChart buckets={series?.buckets ?? []} metric={metric} symbol={symbol} />
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="p-4">
          <CardHeader
            title={`用量归因对照（近 ${attribution?.days ?? WINDOW_PARAMS[attributionWindow].days} 天）`}
            subtitle={
              attribution?.window
                ? `${attribution.window.fromDay || '—'} ~ ${attribution.window.toDay || '—'}`
                : undefined
            }
          />
          {!attribution ? (
            <EmptyState title="暂无归因统计数据" />
          ) : (
            <div className="space-y-3">
              <div className="space-y-1">
                <KeyValueRow
                  label="本控制台已归因 (our)"
                  value={`${symbol}${(attribution.our?.cost ?? 0).toFixed(4)} · ${(attribution.our?.tokens ?? 0).toLocaleString()} tokens · ${attribution.our?.turns ?? 0} 回合`}
                />
                <KeyValueRow
                  label="DSH 全量上游 (dshTotal)"
                  value={`${symbol}${(attribution.dshTotal?.cost ?? 0).toFixed(4)} · ${(attribution.dshTotal?.tokens ?? 0).toLocaleString()} tokens · ${attribution.dshTotal?.events ?? 0} 事件`}
                />
                <KeyValueRow
                  label="未归因差额 (unattributed)"
                  value={`${symbol}${(attribution.unattributed?.cost ?? 0).toFixed(4)} · ${(attribution.unattributed?.tokens ?? 0).toLocaleString()} tokens`}
                />
              </div>
              {attribution.unattributed?.note && (
                <p className="rounded bg-[var(--bg-subtle)] px-2.5 py-1.5 text-[11px] text-[var(--text-muted)]">
                  {attribution.unattributed.note}
                </p>
              )}
            </div>
          )}
        </Card>

        <Card className="p-4">
          <CardHeader
            title="按模型提供商分布"
            subtitle={`总样本 ${byProvider?.totalSamples ?? 0} · 合计 ${symbol}${(byProvider?.totalCost ?? 0).toFixed(4)}`}
          />
          {!byProvider || byProvider.providers.length === 0 ? (
            <EmptyState title="暂无提供商明细" />
          ) : (
            <div className="space-y-2.5">
              {byProvider.providers.map((p) => (
                <div
                  key={p.provider}
                  className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-2.5 text-xs"
                >
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex items-center gap-2">
                      <span className="font-mono font-semibold text-[var(--text-primary)]">
                        {p.provider}
                      </span>
                      {p.estimated && <Badge tone="warn">估算</Badge>}
                    </div>
                    <span className="font-mono font-semibold text-[var(--text-primary)]">
                      {symbol}
                      {p.cost.toFixed(4)} ({p.samples} 样本)
                    </span>
                  </div>
                  <div className="mt-1 text-[11px] text-[var(--text-muted)]">
                    总 Tokens {(p.tokens?.total ?? 0).toLocaleString()} · 命中{' '}
                    {(p.tokens?.cacheHit ?? 0).toLocaleString()} · 未命中{' '}
                    {(p.tokens?.cacheMiss ?? 0).toLocaleString()} · 输出{' '}
                    {(p.tokens?.out ?? 0).toLocaleString()}
                  </div>
                  {p.models && p.models.length > 0 && (
                    <div className="mt-1.5 flex flex-wrap gap-1.5">
                      {p.models.map((m) => (
                        <Badge key={m.model} tone="neutral">
                          {m.model}: {symbol}
                          {m.cost.toFixed(4)} ({m.samples})
                        </Badge>
                      ))}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>

      <Card className="p-4">
        <CardHeader
          title="按会话花费排行与回合钻取"
          subtitle="点击列表中的任一条对话，即可在下方展开查看该对话各回合的详细消耗账目"
          actions={<Badge tone="neutral">{summary?.conversations.length ?? 0} 个会话</Badge>}
        />
        {!summary || summary.conversations.length === 0 ? (
          <EmptyState title="暂无会话花费记录" />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-xs">
              <thead>
                <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                  <th className="py-2 pr-3 font-medium">会话</th>
                  <th className="py-2 pr-3 font-medium">回合 / 步数</th>
                  <th className="py-2 pr-3 font-medium">花费</th>
                  <th className="py-2 pr-3 font-medium">最近活动</th>
                  <th className="py-2 text-right font-medium">操作</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--border-hairline)]">
                {summary.conversations.map((conv) => {
                  const active = selectedConvKey === conv.key;
                  return (
                    <tr
                      key={conv.key}
                      onClick={() => void loadConvTurns(conv.key)}
                      className={`cursor-pointer transition-colors hover:bg-[var(--bg-subtle)]/60 ${
                        active ? 'bg-[var(--accent-soft)]' : ''
                      }`}
                    >
                      <td className="py-2 pr-3">
                        <div className="font-medium text-[var(--text-primary)]">
                          {convNameMap.get(conv.key) || conv.key}
                        </div>
                        {convNameMap.get(conv.key) && (
                          <div className="font-mono text-[10.5px] text-[var(--text-muted)]">
                            {conv.key}
                          </div>
                        )}
                      </td>
                      <td className="py-2 pr-3 font-mono text-[11px] text-[var(--text-secondary)]">
                        {conv.turns} / {conv.steps}
                      </td>
                      <td className="py-2 pr-3 font-mono font-semibold text-[var(--text-primary)]">
                        {symbol}
                        {conv.cost.toFixed(4)}
                      </td>
                      <td className="py-2 pr-3 font-mono text-[11px] text-[var(--text-muted)]">
                        {formatDateTimeMs(conv.lastAt)}
                      </td>
                      <td className="py-2 text-right">
                        <button
                          type="button"
                          onClick={(e) => {
                            e.stopPropagation();
                            setInspector({ kind: 'conversation', id: conv.key });
                          }}
                          className="font-medium text-[var(--accent)] hover:underline"
                        >
                          检视会话
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}

        {selectedConvKey && (
          <div className="mt-4 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3">
            <div className="mb-2 flex items-center justify-between gap-2">
              <div className="text-xs font-semibold text-[var(--text-primary)]">
                回合明细 · <span className="font-mono">{selectedConvKey}</span>
              </div>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setSelectedConvKey(null);
                  setTurns([]);
                  setTurnsError(null);
                }}
              >
                收起明细
              </Button>
            </div>
            {loadingTurns ? (
              <EmptyState title="加载回合花费明细中…" />
            ) : turnsError ? (
              <EmptyState title="加载回合明细失败" description={turnsError} />
            ) : turns.length === 0 ? (
              <EmptyState title="该会话暂无回合花费明细" />
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full text-left text-[11px]">
                  <thead>
                    <tr className="border-b border-[var(--border-hairline)] text-[var(--text-muted)]">
                      <th className="py-1.5 pr-3 font-medium">Turn</th>
                      <th className="py-1.5 pr-3 font-medium">模型 / 提供商</th>
                      <th className="py-1.5 pr-3 font-medium">步 / 重试</th>
                      <th className="py-1.5 pr-3 font-medium">档位</th>
                      <th className="py-1.5 pr-3 font-medium">花费</th>
                      <th className="py-1.5 font-medium">开始时间</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-[var(--border-hairline)]">
                    {turns.map((t, idx) => (
                      <tr key={`${t.sessionId || 's'}-${t.turn}-${idx}`}>
                        <td className="py-1.5 pr-3 font-mono text-[var(--text-primary)]">
                          {t.turn}
                        </td>
                        <td className="py-1.5 pr-3 font-mono text-[var(--text-secondary)]">
                          {t.model || '—'} {t.provider ? `(${t.provider})` : ''}
                        </td>
                        <td className="py-1.5 pr-3 font-mono text-[var(--text-secondary)]">
                          {t.steps ?? 0} / {t.attempts ?? 0}
                        </td>
                        <td className="py-1.5 pr-3">{t.tier || '—'}</td>
                        <td className="py-1.5 pr-3 font-mono font-semibold text-[var(--text-primary)]">
                          {symbol}
                          {t.cost.toFixed(4)}
                        </td>
                        <td className="py-1.5 font-mono text-[var(--text-muted)]">
                          {formatDateTimeMs(t.startedAt)}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="最近用量流水（Usage fact）"
          subtitle="最新 50 条模型调用 Token 与单次计费明细"
          actions={<Badge tone="neutral">{recent.length} 条</Badge>}
        />
        {recent.length === 0 ? (
          <EmptyState title="暂无最近调用流水" />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-[11.5px]">
              <thead>
                <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                  <th className="py-1.5 pr-3 font-medium">时间</th>
                  <th className="py-1.5 pr-3 font-medium">会话</th>
                  <th className="py-1.5 pr-3 font-medium">模型</th>
                  <th className="py-1.5 pr-3 font-medium">Tokens（命中/未命中/输出）</th>
                  <th className="py-1.5 pr-3 font-medium">花费</th>
                  <th className="py-1.5 font-medium">档位</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--border-hairline)]">
                {recent.map((row, idx) => (
                  <tr key={`${row.time ?? idx}-${row.sessionId || ''}-${row.step ?? idx}`}>
                    <td className="py-1.5 pr-3 font-mono text-[var(--text-muted)]">
                      {formatDateTimeMs(row.time)}
                    </td>
                    <td className="py-1.5 pr-3 font-mono text-[var(--text-secondary)]">
                      {row.key || '—'}
                    </td>
                    <td className="py-1.5 pr-3 font-mono text-[var(--text-primary)]">
                      {row.model || '—'}
                    </td>
                    <td className="py-1.5 pr-3 font-mono text-[var(--text-secondary)]">
                      {(row.tokens ?? 0).toLocaleString()} ({row.cacheHit ?? 0}/{row.cacheMiss ?? 0}/
                      {row.output ?? 0})
                    </td>
                    <td className="py-1.5 pr-3 font-mono font-semibold text-[var(--text-primary)]">
                      {symbol}
                      {(row.cost ?? 0).toFixed(5)}
                      {row.estimated ? ' *' : ''}
                    </td>
                    <td className="py-1.5">{row.tier || '—'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      {summary?.priceTable && (
        <Card className="p-4">
          <details>
            <summary className="cursor-pointer text-xs font-semibold text-[var(--text-primary)]">
              当前计费价目表（版本 {summary.priceTable.version || summary.priceVersion || '—'} · 峰时倍率{' '}
              {summary.priceTable.peakMultiplier ?? 1}x）
            </summary>
            <div className="mt-3 space-y-2 text-xs text-[var(--text-secondary)]">
              <div>
                计费单位: {formatTokenUnit(summary.priceTable.unit)} · 峰时时段:{' '}
                {formatPeakWindows(summary.priceTable.peakWindows)} · 峰时星期:{' '}
                {formatPeakWeekdays(summary.priceTable.peakWeekdays)} · 节假日:{' '}
                {summary.priceTable.holidayCount ?? 0} 天
              </div>
              {priceModels.length > 0 && (
                <pre className="overflow-x-auto rounded bg-[var(--bg-subtle)] p-2.5 font-mono text-[11px]">
                  {JSON.stringify(summary.priceTable.models, null, 2)}
                </pre>
              )}
            </div>
          </details>
        </Card>
      )}

      <Card danger className="p-4">
        <CardHeader
          title="危险操作"
          subtitle="清空统计账本中的全部 Token 与花费记录"
          actions={<AlertTriangle className="h-4 w-4 text-[var(--danger)]" />}
        />
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-xs leading-relaxed text-[var(--text-secondary)]">
            注意：在选定运行时模式下，POST /api/tokens/reset 属于受限制端点，执行将返回 HTTP 503 并提示不可用。
          </p>
          <Button size="sm" variant="danger" onClick={handleResetLedger}>
            重置用量账本
          </Button>
        </div>
      </Card>
    </div>
  );
};
