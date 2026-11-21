import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Plus, RefreshCw, RotateCcw, Search } from 'lucide-react';
import {
  LiveRestartInfo,
  PersistedLiveRestart,
  changeConversationScope,
  fetchAllStates,
  fetchConversationSettings,
  fetchInboundGaps,
  fetchModelRoutes,
  fetchPersonasList,
  fetchSessions,
  fetchTurns,
  persistLiveRestartBanner,
  readPersistedLiveRestart,
  triggerWatchdogTask,
} from '../api/run';
import { Badge, Button, Card, PageHeader } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';

interface MergedConversationRow {
  key: string;
  name?: string;
  kind: 'group' | 'private';
  admitted: boolean;
  served: boolean;
  engineToday: string;
  route: string | null;
  routeWarning: boolean;
  personaName: string;
  isDefaultPersona: boolean;
  wakeMode: string;
  unreadCount: number;
  hasSession: boolean;
  pausedTurns: number;
  unresolvedGaps: number;
  recoveryFailed: boolean;
}

const SCOPE_EVENT = 'her-scope-updated';

const PARTICIPATE_MODE_SHORT: Record<string, string> = {
  reserved2: '二代仿真',
  chat: '普通聊天',
  reserved: '一代仿真',
  'closed-agent': '管理员代理',
};

export const ConversationsPage: React.FC = () => {
  const { conversations: bootstrapConversations, globalMode, setInspector, openConfirm, pushToast, refreshLiveData } =
    useConsole();

  const [loading, setLoading] = useState(true);
  const [loadNotice, setLoadNotice] = useState<string | null>(null);
  const [rows, setRows] = useState<MergedConversationRow[]>([]);
  const [search, setSearch] = useState('');
  const [filterKind, setFilterKind] = useState<'all' | 'group' | 'private' | 'alerts'>('all');

  const [addingOpen, setAddingOpen] = useState(false);
  const [newKind, setNewKind] = useState<'group' | 'private'>('group');
  const [newId, setNewId] = useState('');
  const [busyAction, setBusyAction] = useState(false);

  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const [restartState, setRestartState] = useState<PersistedLiveRestart | null>(() =>
    readPersistedLiveRestart(),
  );

  const persistRestartState = (next: PersistedLiveRestart | null) => {
    setRestartState(
      next ? persistLiveRestartBanner(next.info, next.watchdogRunId) : persistLiveRestartBanner(null),
    );
  };

  const nameByKey = useMemo(() => {
    const map = new Map<string, string>();
    for (const c of bootstrapConversations) {
      if (c.key && c.name) {
        map.set(c.key, c.name);
      }
    }
    return map;
  }, [bootstrapConversations]);

  const loadAll = useCallback(async () => {
    const seq = ++loadSeqRef.current;
    setLoading(true);
    const [convSettingsRes, modelsRes, statesRes, personasRes, sessionsRes, turnsRes, gapsRes] =
      await Promise.allSettled([
        fetchConversationSettings(),
        fetchModelRoutes(),
        fetchAllStates(),
        fetchPersonasList(),
        fetchSessions(),
        fetchTurns(),
        fetchInboundGaps(),
      ]);

    if (!mountedRef.current || seq !== loadSeqRef.current) return;

    const warnings: string[] = [];

    const keySet = new Set<string>();
    const settingsMap = new Map<string, { admitted: boolean; engineToday: string }>();
    if (convSettingsRes.status === 'fulfilled') {
      if (convSettingsRes.value.storeError) {
        warnings.push(`设置存储异常：${convSettingsRes.value.storeError}`);
      }
      for (const item of convSettingsRes.value.conversations) {
        keySet.add(item.conversation);
        settingsMap.set(item.conversation, {
          admitted: item.admitted,
          engineToday: item.engineToday,
        });
      }
    } else {
      warnings.push('单会话设置读取失败');
    }

    const modelsMap = new Map<string, { route: string | null; inChain: boolean; order: string[] }>();
    if (modelsRes.status === 'fulfilled') {
      if (modelsRes.value.error) warnings.push(`模型链配置异常：${modelsRes.value.error}`);
      if (modelsRes.value.storeError) warnings.push(`路由存储异常：${modelsRes.value.storeError}`);
      for (const item of modelsRes.value.conversations) {
        keySet.add(item.conversation);
        modelsMap.set(item.conversation, {
          route: item.route,
          inChain: item.inChain,
          order: item.order,
        });
      }
    } else {
      warnings.push('模型路由读取失败');
    }

    const statesMap = new Map<string, { wakeMode: string; unreadCount: number }>();
    if (statesRes.status === 'fulfilled') {
      for (const item of statesRes.value.conversations) {
        keySet.add(item.key);
        statesMap.set(item.key, {
          wakeMode: item.wakeConfig?.mode || '默认',
          unreadCount: item.unreadCount ?? 0,
        });
      }
    } else {
      warnings.push('唤醒状态读取失败');
    }

    const personaNameById = new Map<string, string>();
    let defaultPersonaId: string | null = null;
    let overrides: Record<string, string> = {};
    if (personasRes.status === 'fulfilled') {
      defaultPersonaId = personasRes.value.default;
      overrides = personasRes.value.overrides || {};
      for (const p of personasRes.value.personas) {
        personaNameById.set(p.id, p.name);
      }
      for (const k of Object.keys(overrides)) {
        keySet.add(k);
      }
    } else {
      warnings.push('人设列表读取失败');
    }

    const sessionKeys = new Set<string>();
    if (sessionsRes.status === 'fulfilled') {
      for (const s of sessionsRes.value.sessions) {
        keySet.add(s.key);
        sessionKeys.add(s.key);
      }
    } else {
      warnings.push('会话映射读取失败');
    }

    const pausedByKey = new Map<string, number>();
    let turnsFailed = false;
    if (turnsRes.status === 'fulfilled') {
      for (const p of turnsRes.value.paused) {
        keySet.add(p.key);
        pausedByKey.set(p.key, (pausedByKey.get(p.key) ?? 0) + 1);
      }
    } else {
      turnsFailed = true;
      warnings.push('轮次状态读取失败');
    }

    const gapsByKey = new Map<string, number>();
    let gapsFailed = false;
    if (gapsRes.status === 'fulfilled') {
      for (const g of gapsRes.value.unresolvedGaps) {
        keySet.add(g.conversation);
        gapsByKey.set(g.conversation, (gapsByKey.get(g.conversation) ?? 0) + 1);
      }
    } else {
      gapsFailed = true;
      warnings.push('入站缺口读取失败');
    }

    const recoveryFailed = turnsFailed || gapsFailed;

    const merged: MergedConversationRow[] = Array.from(keySet)
      .sort()
      .map((key) => {
        const settingRow = settingsMap.get(key);
        const modelRow = modelsMap.get(key);
        const stateRow = statesMap.get(key);
        const overrideId = overrides[key];
        const effectivePersonaId = overrideId || defaultPersonaId;
        const personaName = effectivePersonaId
          ? personaNameById.get(effectivePersonaId) || effectivePersonaId
          : '未指定';

        const route = modelRow?.route ?? null;
        return {
          key,
          name: nameByKey.get(key),
          kind: key.startsWith('private:') ? 'private' : 'group',
          admitted: settingRow?.admitted ?? false,
          served: Boolean(modelRow || stateRow || sessionKeys.has(key)),
          engineToday: settingRow?.engineToday || '—',
          route,
          routeWarning: Boolean(route !== null && modelRow && !modelRow.inChain),
          personaName,
          isDefaultPersona: !overrideId,
          wakeMode: stateRow?.wakeMode || '—',
          unreadCount: stateRow?.unreadCount ?? 0,
          hasSession: sessionKeys.has(key),
          pausedTurns: pausedByKey.get(key) ?? 0,
          unresolvedGaps: gapsByKey.get(key) ?? 0,
          recoveryFailed,
        };
      });

    setRows(merged);
    setLoadNotice(warnings.length > 0 ? warnings.join('；') : null);
    setLoading(false);
  }, [nameByKey]);

  useEffect(() => {
    void loadAll();
    const onScopeUpdated = (ev: Event) => {
      const detail = (ev as CustomEvent<{ liveRestart?: LiveRestartInfo }>).detail;
      if (detail?.liveRestart?.state === 'required') {
        persistRestartState({ info: detail.liveRestart, watchdogRunId: null });
      }
      void loadAll();
      void refreshLiveData();
    };
    window.addEventListener(SCOPE_EVENT, onScopeUpdated);
    return () => window.removeEventListener(SCOPE_EVENT, onScopeUpdated);
  }, [loadAll, refreshLiveData]);

  const handleAdmitConversation = () => {
    const cleanId = newId.trim();
    if (!/^[1-9][0-9]*$/.test(cleanId)) {
      pushToast('请输入有效的正整数 QQ 号或群号', 'warn');
      return;
    }
    const targetKey = `${newKind}:${cleanId}`;
    openConfirm({
      title: `确认接入会话 ${targetKey}`,
      description:
        '此操作将把该会话加入允许列表（allow）和实时服务列表（rollout.rustGroups），属于安全访问变更。',
      confirmLabel: '确认接入',
      danger: false,
      onConfirm: async () => {
        setBusyAction(true);
        try {
          const res = await changeConversationScope(targetKey, 'admit', true);
          pushToast(`已接入会话 ${res.conversation}`, 'success');
          if (res.mirrorError) {
            pushToast(`配置回写告警：${res.mirrorError}`, 'warn');
          }
          if (res.liveRestart?.state === 'required') {
            persistRestartState({ info: res.liveRestart, watchdogRunId: null });
          }
          setAddingOpen(false);
          setNewId('');
          await Promise.all([loadAll(), refreshLiveData()]);
          setInspector({ kind: 'conversation', id: targetKey });
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '接入会话失败', 'danger');
        } finally {
          if (mountedRef.current) setBusyAction(false);
        }
      },
    });
  };

  const handleRunWatchdogNow = () => {
    if (!restartState) return;
    openConfirm({
      title: '立即运行监督进程重载实时范围？',
      description:
        '将触发监督进程 watchdog 任务（POST /api/tasks/watchdog/run），按最新允许范围重启实时服务进程；正在进行的回合会先安全收尾。',
      confirmLabel: '立即运行监督进程应用',
      danger: true,
      onConfirm: async () => {
        setBusyAction(true);
        try {
          const res = await triggerWatchdogTask();
          const runId = res.runId || '已排队';
          persistRestartState({ info: restartState.info, watchdogRunId: runId });
          pushToast(`已触发监督进程巡检任务（${runId}），实时服务范围即将同步`, 'success');
        } catch (err) {
          pushToast(err instanceof Error ? err.message : '触发监督进程任务失败', 'danger');
        } finally {
          if (mountedRef.current) setBusyAction(false);
        }
      },
    });
  };

  const filteredRows = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return rows.filter((row) => {
      if (filterKind === 'group' && row.kind !== 'group') return false;
      if (filterKind === 'private' && row.kind !== 'private') return false;
      if (
        filterKind === 'alerts' &&
        row.pausedTurns === 0 &&
        row.unresolvedGaps === 0 &&
        !row.recoveryFailed
      ) {
        return false;
      }
      if (!needle) return true;
      return (
        row.key.toLowerCase().includes(needle) ||
        (row.name || '').toLowerCase().includes(needle) ||
        row.personaName.toLowerCase().includes(needle) ||
        (row.route || '').toLowerCase().includes(needle)
      );
    });
  }, [rows, search, filterKind]);

  const modeLabel = PARTICIPATE_MODE_SHORT[globalMode] || globalMode || '—';

  return (
    <div className="space-y-4">
      <PageHeader
        title="会话"
        description="全量会话工作台与单会话控制总入口：点击任意会话行可打开右侧检视抽屉调整互动、恢复、人设与路由。"
        actions={
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="secondary"
              disabled={loading}
              onClick={() => void loadAll()}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              刷新
            </Button>
            <Button
              size="sm"
              variant="primary"
              onClick={() => setAddingOpen((prev) => !prev)}
            >
              <Plus className="h-3.5 w-3.5" />
              添加会话
            </Button>
          </div>
        }
      />

      {loadNotice && (
        <Card className="px-4 py-2.5 text-xs text-[var(--warn)]">{loadNotice}</Card>
      )}

      {restartState?.info.state === 'required' && (
        <Card className="flex flex-wrap items-center justify-between gap-3 border-amber-300 bg-[var(--warn-soft)] px-4 py-3 text-xs dark:border-amber-900">
          <div className="flex items-start gap-2.5 text-[var(--warn)]">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            <div className="space-y-1">
              <div className="font-semibold">
                {restartState.watchdogRunId
                  ? `已触发监督进程应用新会话范围（任务 ${restartState.watchdogRunId}）`
                  : '会话范围已变更，实时服务进程尚未按新范围重启'}
              </div>
              <p className="text-[var(--text-secondary)]">
                实时进程启动时会固定加载当时的会话列表；监督进程每约 2 分钟自动巡检并按新配置重启，也可以点击右侧按钮立即触发监督进程应用。确认实时服务完成同步后可手动关闭此提示。
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="ghost"
              onClick={() => persistRestartState(null)}
            >
              关闭提示
            </Button>
            <Button
              size="sm"
              variant="primary"
              disabled={busyAction}
              onClick={handleRunWatchdogNow}
            >
              <RotateCcw className="h-3.5 w-3.5" />
              立即运行监督进程应用
            </Button>
          </div>
        </Card>
      )}

      {addingOpen && (
        <Card className="p-4">
          <div className="flex flex-wrap items-end gap-3 text-xs">
            <label className="w-36 space-y-1">
              <span className="block text-[var(--text-secondary)]">会话类型</span>
              <select
                value={newKind}
                onChange={(e) => setNewKind(e.target.value === 'private' ? 'private' : 'group')}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
              >
                <option value="group">QQ 群 (group)</option>
                <option value="private">QQ 私聊 (private)</option>
              </select>
            </label>
            <label className="min-w-[200px] flex-1 space-y-1">
              <span className="block text-[var(--text-secondary)]">QQ 群号 / 用户 QQ 号</span>
              <input
                type="text"
                value={newId}
                onChange={(e) => setNewId(e.target.value)}
                placeholder="输入纯数字 ID，例如 123456"
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
              />
            </label>
            <div className="flex items-center gap-2">
              <Button size="sm" variant="secondary" onClick={() => setAddingOpen(false)}>
                取消
              </Button>
              <Button
                size="sm"
                variant="primary"
                disabled={busyAction}
                onClick={handleAdmitConversation}
              >
                确认接入
              </Button>
            </div>
          </div>
        </Card>
      )}

      <Card>
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-[var(--border-hairline)] px-4 py-2.5">
          <div className="flex items-center gap-1.5 text-xs">
            {(
              [
                ['all', '全部'],
                ['group', '群聊'],
                ['private', '私聊'],
                ['alerts', '有告警'],
              ] as const
            ).map(([id, label]) => (
              <button
                key={id}
                type="button"
                onClick={() => setFilterKind(id)}
                className={`rounded-md px-2.5 py-1 text-xs transition-colors ${
                  filterKind === id
                    ? 'bg-[var(--accent-soft)] font-medium text-[var(--accent)]'
                    : 'text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]'
                }`}
              >
                {label}
              </button>
            ))}
          </div>

          <label className="relative w-full sm:w-64">
            <Search className="pointer-events-none absolute left-2.5 top-2 h-3.5 w-3.5 text-[var(--text-muted)]" />
            <input
              type="search"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder="搜索会话名、ID、人设、路由…"
              className="h-7.5 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] pl-8 pr-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
            />
          </label>
        </div>

        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-xs">
            <thead className="bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
              <tr>
                <th className="px-4 py-2.5 font-medium">会话</th>
                <th className="px-3 py-2.5 font-medium">范围 / 参与模式</th>
                <th className="px-3 py-2.5 font-medium">唤醒 / 未读</th>
                <th className="px-3 py-2.5 font-medium">人设</th>
                <th className="px-3 py-2.5 font-medium">模型路由</th>
                <th className="px-3 py-2.5 font-medium">回合引擎</th>
                <th className="px-3 py-2.5 font-medium">恢复告警</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--border-hairline)]">
              {filteredRows.length === 0 ? (
                <tr>
                  <td colSpan={7} className="px-4 py-8 text-center text-[var(--text-muted)]">
                    {loading ? '加载会话列表中…' : '暂无匹配的会话记录'}
                  </td>
                </tr>
              ) : (
                filteredRows.map((row) => (
                  <tr
                    key={row.key}
                    onClick={() => setInspector({ kind: 'conversation', id: row.key })}
                    className="cursor-pointer transition-colors hover:bg-[var(--bg-hover)]"
                  >
                    <td className="px-4 py-2.5">
                      <div className="font-medium text-[var(--text-primary)]">
                        {row.name || <span className="font-mono">{row.key}</span>}
                      </div>
                      <div className="text-[11px] text-[var(--text-muted)]">
                        {row.name && <span className="mr-1.5 font-mono">{row.key}</span>}
                        <span>{row.kind === 'group' ? '群聊' : '私聊'}</span>
                        {row.hasSession ? ' · 已建立会话映射' : ''}
                      </div>
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="flex flex-wrap items-center gap-1">
                        <Badge tone={row.admitted ? 'success' : 'neutral'}>
                          {row.admitted ? '已允许' : '未允许'}
                        </Badge>
                        <Badge tone="neutral">{modeLabel}</Badge>
                      </div>
                    </td>
                    <td className="px-3 py-2.5 tabular-nums">
                      <span>{row.wakeMode}</span>
                      {row.unreadCount > 0 && (
                        <Badge tone="accent" className="ml-1.5">
                          {row.unreadCount} 未读
                        </Badge>
                      )}
                    </td>
                    <td className="px-3 py-2.5">
                      <span>{row.personaName}</span>
                      {row.isDefaultPersona && (
                        <span className="ml-1 text-[11px] text-[var(--text-muted)]">(默认)</span>
                      )}
                    </td>
                    <td className="px-3 py-2.5 font-mono">
                      <span>{row.route || '默认顺序'}</span>
                      {row.routeWarning && (
                        <Badge tone="warn" className="ml-1.5">
                          不在链内
                        </Badge>
                      )}
                    </td>
                    <td className="px-3 py-2.5 font-mono">
                      <Badge tone={row.engineToday === 'harness' ? 'accent' : 'neutral'}>
                        {row.engineToday}
                      </Badge>
                    </td>
                    <td className="px-3 py-2.5">
                      {row.recoveryFailed ? (
                        <Badge tone="warn">读取失败</Badge>
                      ) : row.pausedTurns === 0 && row.unresolvedGaps === 0 ? (
                        <span className="text-[var(--text-muted)]">正常</span>
                      ) : (
                        <div className="flex flex-wrap gap-1">
                          {row.pausedTurns > 0 && (
                            <Badge tone="danger">{row.pausedTurns} 暂停轮次</Badge>
                          )}
                          {row.unresolvedGaps > 0 && (
                            <Badge tone="warn">{row.unresolvedGaps} 缺口</Badge>
                          )}
                        </div>
                      )}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Card>
    </div>
  );
};
