import React, { useMemo, useState } from 'react';
import { CalendarClock, History, Pause, Play, Search, Trash2 } from 'lucide-react';
import {
  ConversationStateSummary,
  SettingItemView,
} from '../../api/client';
import {
  ScheduleActionName,
  ScheduleEntry,
  ScheduleHistoryRow,
  ScheduleRepeat,
  createScheduleReminder,
  makeRequestId,
  runScheduleAction,
} from '../../api/system';
import { Badge, Button, Card, CardHeader, SettingField } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import {
  RuntimeZoneInfo,
  formatErrorMessage,
  formatRuntimeTime,
  unicodeScalarLength,
} from '../../utils/systemFormat';

const KIND_LABELS: Record<string, string> = {
  job: '系统任务',
  reminder: '提醒',
  sleep: '潜水定时',
  replyCheck: '回复检查',
  reachOut: '主动冒泡',
};

interface UpcomingScheduleTabProps {
  loading: boolean;
  busy: boolean;
  setBusy: React.Dispatch<React.SetStateAction<boolean>>;
  entries: ScheduleEntry[];
  history: ScheduleHistoryRow[];
  timeZone: RuntimeZoneInfo | null;
  conversations: ConversationStateSummary[];
  scheduleSettings: SettingItemView[];
  dirtyDrafts: Record<string, unknown>;
  onDraftChange: (key: string, value: unknown) => void;
  onResetSetting: (setting: SettingItemView) => void;
  createOpen: boolean;
  setCreateOpen: React.Dispatch<React.SetStateAction<boolean>>;
  createGenRef: React.MutableRefObject<number>;
  mountedRef: React.MutableRefObject<boolean>;
  onRefreshUpcoming: () => Promise<void>;
}

export const UpcomingScheduleTab: React.FC<UpcomingScheduleTabProps> = ({
  loading,
  busy,
  setBusy,
  entries,
  history,
  timeZone,
  conversations,
  scheduleSettings,
  dirtyDrafts,
  onDraftChange,
  onResetSetting,
  createOpen,
  setCreateOpen,
  createGenRef,
  mountedRef,
  onRefreshUpcoming,
}) => {
  const { openConfirm, pushToast } = useConsole();

  const [kindFilter, setKindFilter] = useState<string>('all');
  const [search, setSearch] = useState('');

  const [remConv, setRemConv] = useState('');
  const [remText, setRemText] = useState('');
  const [remLocalAt, setRemLocalAt] = useState('');
  const [remRepeatKind, setRemRepeatKind] = useState<'once' | 'every' | 'daily'>('once');
  const [remEveryMinutes, setRemEveryMinutes] = useState('60');
  const [remDailyHour, setRemDailyHour] = useState('9');
  const [remDailyMinute, setRemDailyMinute] = useState('0');

  const [editingEntry, setEditingEntry] = useState<ScheduleEntry | null>(null);
  const [editText, setEditText] = useState('');
  const [editLocalAt, setEditLocalAt] = useState('');

  const tabMountedRef = React.useRef(true);
  React.useEffect(() => {
    tabMountedRef.current = true;
    return () => {
      tabMountedRef.current = false;
    };
  }, []);
  const editGenRef = React.useRef(0);

  const zoneLabel = timeZone?.label || '未配置';

  const handleScheduleAction = (entry: ScheduleEntry, action: ScheduleActionName) => {
    if (busy) return;
    if (action === 'edit') {
      setEditingEntry(entry);
      setEditText(entry.action || '');
      setEditLocalAt('');
      return;
    }

    const execute = async () => {
      setBusy(true);
      try {
        const body: Record<string, unknown> = {};
        if (entry.creator === 'system') {
          body.confirm = true;
          if (action === 'run') {
            body.requestId = makeRequestId(`sched-${entry.id}`);
          }
        } else if (
          (entry.kind === 'replyCheck' || entry.kind === 'reachOut') &&
          action === 'cancel'
        ) {
          if (typeof entry.nextRunAt !== 'number') {
            pushToast('缺少该计时器的计划触发时间戳，请先刷新列表', 'warn');
            return;
          }
          body.at = entry.nextRunAt;
        }
        await runScheduleAction(entry.id, action, body);
        pushToast(`已提交操作：${entry.id} -> ${action}`, 'success');
        await onRefreshUpcoming();
      } catch (err) {
        pushToast(formatErrorMessage(err, '执行日程操作失败'), 'danger');
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    };

    const needsConfirm = entry.creator === 'system' || action === 'cancel';
    if (needsConfirm) {
      openConfirm({
        title: `确认对 ${entry.id} 执行「${action}」？`,
        description:
          entry.kind === 'reachOut' && action === 'cancel'
            ? '取消本次主动冒泡机会后，下一次机会会在之后重新安排。'
            : entry.creator === 'system'
              ? '系统任务控制需要显式确认。'
              : '此操作将取消该条日程/计时器，无法撤回。',
        confirmLabel: '确认执行',
        danger: action === 'cancel',
        onConfirm: execute,
      });
      return;
    }
    void execute();
  };

  const handleSaveEdit = async () => {
    if (!editingEntry || busy) return;
    const body: Record<string, unknown> = {};
    if (editingEntry.kind === 'reminder') {
      const trimmedText = editText.trim();
      if (!trimmedText) {
        pushToast('提醒内容不能为空', 'warn');
        return;
      }
      if (unicodeScalarLength(trimmedText) > 500) {
        pushToast('提醒内容最多 500 个 Unicode 字符', 'warn');
        return;
      }
      body.text = trimmedText;
      if (editLocalAt.trim()) body.localAt = editLocalAt.trim();
    } else if (editingEntry.kind === 'sleep') {
      if (!editLocalAt.trim()) {
        pushToast('修改潜水结束时间需选择目标本地时间', 'warn');
        return;
      }
      body.localAt = editLocalAt.trim();
    }
    const startEditGen = editGenRef.current;
    setBusy(true);
    try {
      await runScheduleAction(editingEntry.id, 'edit', body);
      pushToast(`已更新日程 ${editingEntry.id}`, 'success');
      if (mountedRef.current && editGenRef.current === startEditGen) {
        setEditingEntry(null);
      }
      await onRefreshUpcoming();
    } catch (err) {
      pushToast(formatErrorMessage(err, '更新日程失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleCreateReminder = async () => {
    if (busy) return;
    const conv = remConv.trim();
    const text = remText.trim();
    const localAt = remLocalAt.trim();
    if (!/^(group|private):\d+$/.test(conv)) {
      pushToast('请先选中或填入格式合规的会话标识（群聊 group:<群ID> 或单聊 private:<账号>）', 'warn');
      return;
    }
    const scalarCount = unicodeScalarLength(text);
    if (scalarCount === 0 || scalarCount > 500) {
      pushToast('提醒内容需为 1-500 个 Unicode 字符', 'warn');
      return;
    }
    if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(localAt)) {
      pushToast('请选择有效的执行时间', 'warn');
      return;
    }

    let repeat: ScheduleRepeat = { kind: 'once' };
    if (remRepeatKind === 'every') {
      const mins = Number(remEveryMinutes);
      if (!Number.isInteger(mins) || mins < 15) {
        pushToast('间隔重复至少需要 15 分钟的整数', 'warn');
        return;
      }
      repeat = { kind: 'every', minutes: mins };
    } else if (remRepeatKind === 'daily') {
      if (!remDailyHour.trim() || !remDailyMinute.trim()) {
        pushToast('每日重复需填写有效的小时 (0-23) 与分钟 (0-59)', 'warn');
        return;
      }
      const hour = Number(remDailyHour);
      const minute = Number(remDailyMinute);
      if (
        !Number.isInteger(hour) ||
        hour < 0 ||
        hour > 23 ||
        !Number.isInteger(minute) ||
        minute < 0 ||
        minute > 59
      ) {
        pushToast('每日重复时间需为合法的小时 (0-23) 与分钟 (0-59)', 'warn');
        return;
      }
      repeat = { kind: 'daily', hour, minute };
    }

    const startCreateGen = createGenRef.current;
    setBusy(true);
    try {
      await createScheduleReminder({ conversation: conv, text, localAt, repeat });
      pushToast('已创建新提醒', 'success');
      if (createGenRef.current === startCreateGen) {
        if (tabMountedRef.current) setRemText('');
        if (mountedRef.current) setCreateOpen(false);
      }
      await onRefreshUpcoming();
    } catch (err) {
      pushToast(formatErrorMessage(err, '创建提醒失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const filteredEntries = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return entries.filter((entry) => {
      if (kindFilter !== 'all' && entry.kind !== kindFilter) return false;
      if (!needle) return true;
      return (
        entry.id.toLowerCase().includes(needle) ||
        (entry.conversation || '').toLowerCase().includes(needle) ||
        entry.action.toLowerCase().includes(needle)
      );
    });
  }, [entries, kindFilter, search]);

  return (
    <div className="space-y-4">
      {createOpen && (
        <Card className="p-4 space-y-3 text-xs">
          <div className="font-semibold text-[var(--text-primary)]">创建定时提醒</div>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <label className="space-y-1">
              <span className="block text-[var(--text-secondary)]">目标会话</span>
              <input
                type="text"
                list="sched-conversations"
                value={remConv}
                onChange={(e) => {
                  createGenRef.current += 1;
                  setRemConv(e.target.value);
                }}
                placeholder="group:<群号> 或 private:<QQ号>"
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs"
              />
              <datalist id="sched-conversations">
                {conversations.map((c) => (
                  <option key={c.key} value={c.key} />
                ))}
              </datalist>
            </label>
            <label className="space-y-1">
              <span className="block text-[var(--text-secondary)]">
                首次执行（按运行区 {zoneLabel}）
              </span>
              <input
                type="datetime-local"
                value={remLocalAt}
                onChange={(e) => {
                  createGenRef.current += 1;
                  setRemLocalAt(e.target.value);
                }}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
              />
            </label>
            <label className="space-y-1">
              <span className="block text-[var(--text-secondary)]">重复规则</span>
              <select
                value={remRepeatKind}
                onChange={(e) => {
                  createGenRef.current += 1;
                  setRemRepeatKind(
                    e.target.value === 'every' || e.target.value === 'daily'
                      ? e.target.value
                      : 'once',
                  );
                }}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
              >
                <option value="once">仅一次</option>
                <option value="every">每隔 N 分钟（≥15）</option>
                <option value="daily">每日定时</option>
              </select>
            </label>
            {remRepeatKind === 'every' && (
              <label className="space-y-1">
                <span className="block text-[var(--text-secondary)]">间隔分钟数</span>
                <input
                  type="number"
                  min={15}
                  value={remEveryMinutes}
                  onChange={(e) => {
                    createGenRef.current += 1;
                    setRemEveryMinutes(e.target.value);
                  }}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
                />
              </label>
            )}
            {remRepeatKind === 'daily' && (
              <div className="grid grid-cols-2 gap-2">
                <label className="space-y-1">
                  <span className="block text-[var(--text-secondary)]">小时 (0-23)</span>
                  <input
                    type="number"
                    min={0}
                    max={23}
                    value={remDailyHour}
                    onChange={(e) => {
                      createGenRef.current += 1;
                      setRemDailyHour(e.target.value);
                    }}
                    className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
                  />
                </label>
                <label className="space-y-1">
                  <span className="block text-[var(--text-secondary)]">分钟 (0-59)</span>
                  <input
                    type="number"
                    min={0}
                    max={59}
                    value={remDailyMinute}
                    onChange={(e) => {
                      createGenRef.current += 1;
                      setRemDailyMinute(e.target.value);
                    }}
                    className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
                  />
                </label>
              </div>
            )}
          </div>
          <label className="block space-y-1">
            <span className="block text-[var(--text-secondary)]">
              提醒内容（{unicodeScalarLength(remText.trim())} / 500 Unicode 字符）
            </span>
            <input
              type="text"
              value={remText}
              onChange={(e) => {
                createGenRef.current += 1;
                setRemText(e.target.value);
              }}
              placeholder="输入到期时投递给 HER 的提醒事项…"
              className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
            />
          </label>
          <div className="flex justify-end gap-2">
            <Button size="sm" variant="ghost" onClick={() => setCreateOpen(false)}>
              取消
            </Button>
            <Button
              size="sm"
              variant="primary"
              disabled={busy}
              onClick={() => void handleCreateReminder()}
            >
              确认创建
            </Button>
          </div>
        </Card>
      )}

      {editingEntry && (
        <Card className="border-blue-300 bg-[var(--accent-soft)]/30 p-4 space-y-3 text-xs dark:border-blue-900">
          <div className="font-semibold text-[var(--text-primary)]">
            编辑日程：{editingEntry.id}
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            {editingEntry.kind === 'reminder' && (
              <label className="space-y-1">
                <span className="block text-[var(--text-secondary)]">
                  提醒文本（{unicodeScalarLength(editText.trim())} / 500）
                </span>
                <input
                  type="text"
                  value={editText}
                  onChange={(e) => {
                    editGenRef.current += 1;
                    setEditText(e.target.value);
                  }}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
                />
              </label>
            )}
            <label className="space-y-1">
              <span className="block text-[var(--text-secondary)]">
                新触发时间（按运行区 {zoneLabel}）
              </span>
              <input
                type="datetime-local"
                value={editLocalAt}
                onChange={(e) => {
                  editGenRef.current += 1;
                  setEditLocalAt(e.target.value);
                }}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
              />
            </label>
          </div>
          <div className="flex justify-end gap-2">
            <Button size="sm" variant="ghost" onClick={() => setEditingEntry(null)}>
              取消
            </Button>
            <Button size="sm" variant="primary" disabled={busy} onClick={() => void handleSaveEdit()}>
              保存修改
            </Button>
          </div>
        </Card>
      )}

      <Card>
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-[var(--border-hairline)] px-4 py-2.5">
          <div className="flex flex-wrap items-center gap-1.5 text-xs">
            {[
              ['all', '全部'],
              ['job', '系统任务'],
              ['reminder', '提醒'],
              ['sleep', '潜水定时'],
              ['replyCheck', '回复检查'],
              ['reachOut', '主动冒泡'],
            ].map(([id, label]) => (
              <button
                key={id}
                type="button"
                onClick={() => setKindFilter(id)}
                className={`rounded-md px-2.5 py-1 text-xs ${
                  kindFilter === id
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
              placeholder="搜索日程 ID、会话或动作…"
              className="h-7.5 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] pl-8 pr-2.5 text-xs"
            />
          </label>
        </div>
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-xs">
            <thead className="bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
              <tr>
                <th className="px-4 py-2.5 font-medium">ID / 类别</th>
                <th className="px-3 py-2.5 font-medium">创建方 / 会话</th>
                <th className="px-3 py-2.5 font-medium">内容 / 动作</th>
                <th className="px-3 py-2.5 font-medium">下次执行 ({zoneLabel}) / 重复</th>
                <th className="px-3 py-2.5 font-medium">状态 / 上次结果</th>
                <th className="px-4 py-2.5 font-medium text-right">操作</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--border-hairline)]">
              {filteredEntries.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-4 py-8 text-center text-[var(--text-muted)]">
                    {loading ? '读取日程中…' : '暂无日程条目'}
                  </td>
                </tr>
              ) : (
                filteredEntries.map((entry) => (
                  <tr key={entry.id}>
                    <td className="px-4 py-2.5">
                      <div className="font-mono font-medium text-[var(--text-primary)]">
                        {entry.id}
                      </div>
                      <div className="text-[11px] text-[var(--text-muted)]">
                        {KIND_LABELS[entry.kind] || entry.kind}
                      </div>
                    </td>
                    <td className="px-3 py-2.5">
                      <Badge tone="neutral">{entry.creator}</Badge>
                      <div className="mt-1 font-mono text-[11px] text-[var(--text-secondary)]">
                        {entry.conversation || '全局'}
                      </div>
                    </td>
                    <td className="max-w-xs px-3 py-2.5 text-[var(--text-primary)] break-all">
                      {entry.action}
                    </td>
                    <td className="px-3 py-2.5 text-[11px]">
                      <div className="tabular-nums text-[var(--text-primary)]">
                        {formatRuntimeTime(entry.nextRunAt, timeZone)}
                      </div>
                      <div className="text-[var(--text-muted)]">{entry.repeat}</div>
                    </td>
                    <td className="px-3 py-2.5">
                      <Badge
                        tone={
                          entry.status === 'running' || entry.status === 'active'
                            ? 'success'
                            : entry.status === 'paused' || entry.status === 'disabled'
                              ? 'neutral'
                              : 'warn'
                        }
                      >
                        {entry.status}
                      </Badge>
                      {entry.lastResult && (
                        <div className="mt-1 text-[11px] text-[var(--text-muted)]">
                          {entry.lastResult.summary}
                        </div>
                      )}
                    </td>
                    <td className="px-4 py-2.5 text-right">
                      <div className="inline-flex flex-wrap justify-end gap-1">
                        {entry.actions.includes('run') &&
                          entry.id !== 'system:console-restart' && (
                            <Button
                              size="sm"
                              variant="secondary"
                              disabled={busy}
                              onClick={() => handleScheduleAction(entry, 'run')}
                            >
                              <Play className="h-3 w-3" />
                              立即运行
                            </Button>
                          )}
                        {entry.actions.includes('pause') && (
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={busy}
                            onClick={() => handleScheduleAction(entry, 'pause')}
                          >
                            <Pause className="h-3 w-3" />
                            暂停
                          </Button>
                        )}
                        {entry.actions.includes('resume') && (
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={busy}
                            onClick={() => handleScheduleAction(entry, 'resume')}
                          >
                            恢复
                          </Button>
                        )}
                        {entry.actions.includes('edit') && (
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={busy}
                            onClick={() => handleScheduleAction(entry, 'edit')}
                          >
                            编辑
                          </Button>
                        )}
                        {entry.actions.includes('cancel') && (
                          <Button
                            size="sm"
                            variant="danger"
                            disabled={busy}
                            onClick={() => handleScheduleAction(entry, 'cancel')}
                          >
                            <Trash2 className="h-3 w-3" />
                            取消
                          </Button>
                        )}
                      </div>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title={
              <span className="inline-flex items-center gap-1.5">
                <History className="h-4 w-4 text-[var(--text-secondary)]" />
                近期提醒历史 ({zoneLabel})
              </span>
            }
            subtitle="已触发提醒与迟到情况摘要"
          />
          <div className="max-h-72 overflow-y-auto divide-y divide-[var(--border-hairline)] text-xs">
            {history.length === 0 ? (
              <div className="px-4 py-6 text-center text-[var(--text-muted)]">暂无提醒执行历史</div>
            ) : (
              history.map((h, i) => (
                <div key={`${h.id}-${h.at}-${i}`} className="px-4 py-2.5 space-y-1">
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-mono text-[var(--text-primary)]">{h.conversation}</span>
                    <span className="tabular-nums text-[11px] text-[var(--text-muted)]">
                      {formatRuntimeTime(h.at, timeZone)}
                    </span>
                  </div>
                  <div className="flex items-center justify-between gap-2">
                    <span className="text-[var(--text-secondary)] break-all">{h.summary}</span>
                    {h.late && <Badge tone="warn">延迟触发</Badge>}
                  </div>
                </div>
              ))
            )}
          </div>
        </Card>

        <Card>
          <CardHeader
            title={
              <span className="inline-flex items-center gap-1.5">
                <CalendarClock className="h-4 w-4 text-[var(--accent)]" />
                日程与时区实时设置
              </span>
            }
            subtitle="对应 schedule 分组注册设置项，修改后通过底部栏统一保存"
          />
          <div className="space-y-4 p-4">
            {scheduleSettings.length === 0 ? (
              <div className="text-xs text-[var(--text-muted)]">未加载到 schedule 设置项</div>
            ) : (
              scheduleSettings.map((setting) => (
                <div
                  key={setting.key}
                  className="rounded border border-[var(--border-hairline)] p-3"
                >
                  <SettingField
                    setting={setting}
                    draftValue={dirtyDrafts[setting.key]}
                    onDraftChange={(val) => onDraftChange(setting.key, val)}
                    onReset={() => onResetSetting(setting)}
                  />
                </div>
              ))
            )}
          </div>
        </Card>
      </div>
    </div>
  );
};
