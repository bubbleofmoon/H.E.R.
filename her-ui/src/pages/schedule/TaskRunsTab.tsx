import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ChevronDown, ChevronUp, X } from 'lucide-react';
import {
  LegacyTaskRow,
  RunView,
  TaskLogLine,
  TaskRow,
  fetchTaskRunLog,
} from '../../api/system';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import {
  RuntimeZoneInfo,
  formatErrorMessage,
  formatRuntimeTime,
} from '../../utils/systemFormat';

interface TaskRunsTabProps {
  busy: boolean;
  tasks: TaskRow[];
  legacyTasks: LegacyTaskRow[];
  taskFilter: string;
  setTaskFilter: React.Dispatch<React.SetStateAction<string>>;
  runs: RunView[];
  runsLoading: boolean;
  nextBefore: number | null;
  timeZone: RuntimeZoneInfo | null;
  followedRun: { runId: string; seq: number } | null;
  onSelectRunLog: (runId: string | null) => void;
  onQuickRunTask: (task: TaskRow) => void;
  taskFilterRef: React.MutableRefObject<string>;
  onLoadRuns: (opts?: { task?: string; before?: number | null; append?: boolean }) => Promise<void>;
  onRefreshUpcoming: () => Promise<void>;
}

function appendUniqueLines(prev: TaskLogLine[], incoming: TaskLogLine[]): TaskLogLine[] {
  if (!incoming.length) return prev;
  const seen = new Set(prev.map((l) => `${l.n}:${l.stream}`));
  const extra = incoming.filter((l) => !seen.has(`${l.n}:${l.stream}`));
  return extra.length ? [...prev, ...extra] : prev;
}

function isActiveRunState(state: string): boolean {
  return state === 'queued' || state === 'running';
}

export const TaskRunsTab: React.FC<TaskRunsTabProps> = ({
  busy,
  tasks,
  legacyTasks,
  taskFilter,
  setTaskFilter,
  runs,
  runsLoading,
  nextBefore,
  timeZone,
  followedRun,
  onSelectRunLog,
  onQuickRunTask,
  taskFilterRef,
  onLoadRuns,
  onRefreshUpcoming,
}) => {
  const { pushToast } = useConsole();

  const expandedRunId = followedRun?.runId ?? null;
  const expandedSeq = followedRun?.seq ?? 0;

  const [logRunState, setLogRunState] = useState<string>('');
  const [logLines, setLogLines] = useState<TaskLogLine[]>([]);
  const [logNextAfter, setLogNextAfter] = useState<number | null>(null);
  const [logLoadingMore, setLogLoadingMore] = useState(false);
  const [stdoutHidden, setStdoutHidden] = useState(false);

  const tabMountedRef = useRef(true);
  const activeRunIdRef = useRef<string | null>(null);
  const logSeqRef = useRef(0);
  const logLastNRef = useRef<number | null>(null);

  useEffect(() => {
    tabMountedRef.current = true;
    return () => {
      tabMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (!expandedRunId) {
      activeRunIdRef.current = null;
      logSeqRef.current += 1;
      logLastNRef.current = null;
      setLogRunState('');
      setLogLines([]);
      setLogNextAfter(null);
      setStdoutHidden(false);
      return undefined;
    }

    const targetRunId = expandedRunId;
    activeRunIdRef.current = targetRunId;
    const seq = ++logSeqRef.current;
    logLastNRef.current = null;
    let pollingStopped = false;
    let pollTimer: number | null = null;

    setLogRunState('');
    setLogLines([]);
    setLogNextAfter(null);
    setStdoutHidden(false);

    const isStale = () =>
      pollingStopped ||
      !tabMountedRef.current ||
      activeRunIdRef.current !== targetRunId ||
      logSeqRef.current !== seq;

    const applyPage = (page: Awaited<ReturnType<typeof fetchTaskRunLog>>) => {
      const incoming = page.lines || [];
      if (incoming.length > 0) {
        const tailN = incoming[incoming.length - 1].n;
        logLastNRef.current = page.nextAfter ?? tailN;
        setLogLines((prev) => appendUniqueLines(prev, incoming));
      } else if (page.nextAfter !== null && page.nextAfter !== undefined) {
        logLastNRef.current = page.nextAfter;
      }
      setLogNextAfter(page.nextAfter ?? null);
      setLogRunState(page.state || '');
    };

    const startPolling = () => {
      if (pollTimer !== null || isStale()) return;
      let pollInFlight = false;
      let terminalHandled = false;
      pollTimer = window.setInterval(async () => {
        if (pollInFlight || terminalHandled || isStale() || document.hidden) return;
        pollInFlight = true;
        try {
          const pollRes = await fetchTaskRunLog(targetRunId, {
            after: logLastNRef.current,
            limit: 200,
          });
          if (terminalHandled || isStale() || pollRes.runId !== targetRunId) return;

          if (pollRes.state && !isActiveRunState(pollRes.state)) {
            terminalHandled = true;
            if (pollTimer !== null) {
              window.clearInterval(pollTimer);
              pollTimer = null;
            }
            const incoming = pollRes.lines || [];
            if (incoming.length > 0) {
              logLastNRef.current = pollRes.nextAfter ?? incoming[incoming.length - 1].n;
            }
            applyPage(pollRes);
            // Best-effort catch up on any tail lines written as the process exited.
            try {
              const tailRes = await fetchTaskRunLog(targetRunId, {
                after: logLastNRef.current,
                limit: 200,
              });
              if (!isStale() && tailRes.runId === targetRunId) {
                applyPage(tailRes);
              }
            } catch {
              // Keep pollRes output even if tail fetch fails
            }
            if (!isStale()) {
              void Promise.all([
                onLoadRuns({ task: taskFilterRef.current || undefined, append: false }),
                onRefreshUpcoming(),
              ]);
            }
            return;
          }

          applyPage(pollRes);
        } catch {
          // Ignore transient polling failures
        } finally {
          pollInFlight = false;
        }
      }, 4000);
    };

    void (async () => {
      try {
        const initRes = await fetchTaskRunLog(targetRunId, { limit: 200 });
        if (isStale() || initRes.runId !== targetRunId) return;
        setStdoutHidden(Boolean(initRes.stdoutHidden));
        applyPage(initRes);
        if (isActiveRunState(initRes.state)) {
          startPolling();
        } else {
          void Promise.all([
            onLoadRuns({ task: taskFilterRef.current || undefined, append: false }),
            onRefreshUpcoming(),
          ]);
        }
      } catch (err) {
        if (!isStale()) {
          pushToast(formatErrorMessage(err, '读取运行日志失败'), 'danger');
        }
      }
    })();

    return () => {
      pollingStopped = true;
      if (pollTimer !== null) {
        window.clearInterval(pollTimer);
      }
    };
  }, [expandedRunId, expandedSeq, onLoadRuns, onRefreshUpcoming, pushToast, taskFilterRef]);

  const handleLoadMoreLog = useCallback(async () => {
    if (!expandedRunId || logLoadingMore) return;
    const cursor = logNextAfter ?? logLastNRef.current;
    if (cursor === null) return;
    const runId = expandedRunId;
    const seq = logSeqRef.current;
    setLogLoadingMore(true);
    try {
      const logRes = await fetchTaskRunLog(runId, { after: cursor, limit: 200 });
      if (
        !tabMountedRef.current ||
        logSeqRef.current !== seq ||
        activeRunIdRef.current !== runId ||
        logRes.runId !== runId
      ) {
        return;
      }
      const incoming = logRes.lines || [];
      const tailN = incoming.length > 0 ? incoming[incoming.length - 1].n : cursor;
      logLastNRef.current = logRes.nextAfter ?? tailN;
      setLogLines((prev) => appendUniqueLines(prev, incoming));
      setLogNextAfter(logRes.nextAfter ?? null);
      setLogRunState(logRes.state || '');
    } catch (err) {
      if (tabMountedRef.current && logSeqRef.current === seq) {
        pushToast(formatErrorMessage(err, '加载后续日志失败'), 'danger');
      }
    } finally {
      if (tabMountedRef.current) setLogLoadingMore(false);
    }
  }, [expandedRunId, logLoadingMore, logNextAfter, pushToast]);

  const toggleRunLog = (runId: string) => {
    onSelectRunLog(expandedRunId === runId ? null : runId);
  };

  const renderLogBody = () => (
    <div className="space-y-2">
      {stdoutHidden && (
        <div className="text-[11px] text-[var(--warn)]">
          注意：涉密或聊天输出任务的 stdout 已隐藏，仅显示元信息事件。
        </div>
      )}
      <div className="max-h-72 overflow-y-auto rounded border border-[var(--border-hairline)] bg-[var(--bg-surface)] p-2.5 font-mono text-[11px] leading-relaxed">
        {logLines.length === 0 ? (
          <span className="text-[var(--text-muted)]">暂无日志行</span>
        ) : (
          logLines.map((line) => (
            <div key={`${line.n}-${line.stream}`} className="break-all">
              <span className="mr-2 text-[var(--text-muted)]">[{line.stream}]</span>
              <span>{line.text}</span>
            </div>
          ))
        )}
      </div>
      {logNextAfter !== null && !isActiveRunState(logRunState) && (
        <div className="flex justify-end">
          <Button
            size="sm"
            variant="secondary"
            disabled={logLoadingMore}
            onClick={() => void handleLoadMoreLog()}
          >
            {logLoadingMore ? '读取中…' : '加载后续日志'}
          </Button>
        </div>
      )}
    </div>
  );

  const expandedOutsideFilteredRows =
    Boolean(expandedRunId) && !runs.some((r) => r.runId === expandedRunId);

  return (
    <div className="space-y-4">
      {expandedOutsideFilteredRows && expandedRunId && (
        <Card className="border-blue-300 dark:border-blue-900">
          <CardHeader
            title={`当前跟踪运行日志：${expandedRunId}`}
            subtitle="该运行实例不在当前筛选列表中，仍持续显示其实时日志"
            actions={
              <div className="flex items-center gap-2">
                {logRunState && <Badge tone="neutral">{logRunState}</Badge>}
                <Button size="sm" variant="ghost" onClick={() => onSelectRunLog(null)}>
                  <X className="h-3.5 w-3.5" />
                  关闭
                </Button>
              </div>
            }
          />
          <div className="p-4">{renderLogBody()}</div>
        </Card>
      )}

      <Card>
        <CardHeader
          title="后台任务运行记录"
          subtitle="查看所有已注册任务的排队、执行结果与实时日志流"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <select
                value={taskFilter}
                onChange={(e) => setTaskFilter(e.target.value)}
                className="h-8 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
              >
                <option value="">全部任务</option>
                {tasks.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.title} ({t.id})
                  </option>
                ))}
              </select>
              {tasks
                .filter((t) => t.id !== 'console-restart')
                .slice(0, 3)
                .map((t) => (
                  <Button
                    key={t.id}
                    size="sm"
                    variant="secondary"
                    disabled={busy || t.running}
                    onClick={() => onQuickRunTask(t)}
                  >
                    运行 {t.title}
                  </Button>
                ))}
            </div>
          }
        />
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-xs">
            <thead className="bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
              <tr>
                <th className="px-4 py-2.5 font-medium">Run ID</th>
                <th className="px-3 py-2.5 font-medium">任务 / 来源</th>
                <th className="px-3 py-2.5 font-medium">状态</th>
                <th className="px-3 py-2.5 font-medium">开始 / 结束</th>
                <th className="px-3 py-2.5 font-medium">退出码 / 原因</th>
                <th className="px-4 py-2.5 font-medium text-right">日志</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--border-hairline)]">
              {runs.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-4 py-8 text-center text-[var(--text-muted)]">
                    {runsLoading ? '读取运行记录中…' : '暂无运行记录'}
                  </td>
                </tr>
              ) : (
                runs.map((run) => {
                  const open = expandedRunId === run.runId;
                  return (
                    <React.Fragment key={run.runId}>
                      <tr>
                        <td className="px-4 py-2.5 font-mono">{run.runId}</td>
                        <td className="px-3 py-2.5">
                          <span className="font-mono font-medium">{run.taskId}</span>
                          <span className="ml-1.5 text-[11px] text-[var(--text-muted)]">
                            ({run.source})
                          </span>
                        </td>
                        <td className="px-3 py-2.5">
                          <Badge
                            tone={
                              run.state === 'succeeded'
                                ? 'success'
                                : run.state === 'failed' || run.state === 'unknown'
                                  ? 'danger'
                                  : run.state === 'running' || run.state === 'queued'
                                    ? 'warn'
                                    : 'neutral'
                            }
                          >
                            {run.state}
                          </Badge>
                        </td>
                        <td className="px-3 py-2.5 tabular-nums text-[11px]">
                          <div>{formatRuntimeTime(run.startedAt ?? run.queuedAt, timeZone)}</div>
                          <div className="text-[var(--text-muted)]">
                            {formatRuntimeTime(run.endedAt, timeZone)}
                          </div>
                        </td>
                        <td className="px-3 py-2.5 text-[11px]">
                          {run.exitCode !== null ? `exit=${run.exitCode}` : ''}
                          {run.reason ? ` ${run.reason}` : ''}
                        </td>
                        <td className="px-4 py-2.5 text-right">
                          <Button
                            size="sm"
                            variant="ghost"
                            onClick={() => toggleRunLog(run.runId)}
                          >
                            {open ? (
                              <>
                                <ChevronUp className="h-3.5 w-3.5" />
                                收起日志
                              </>
                            ) : (
                              <>
                                <ChevronDown className="h-3.5 w-3.5" />
                                查看日志
                              </>
                            )}
                          </Button>
                        </td>
                      </tr>
                      {open && (
                        <tr>
                          <td colSpan={6} className="bg-[var(--bg-subtle)]/50 px-4 py-3">
                            {renderLogBody()}
                          </td>
                        </tr>
                      )}
                    </React.Fragment>
                  );
                })
              )}
            </tbody>
          </table>
        </div>
        {nextBefore !== null && (
          <div className="border-t border-[var(--border-hairline)] px-4 py-2.5 text-center">
            <Button
              size="sm"
              variant="secondary"
              disabled={runsLoading}
              onClick={() =>
                void onLoadRuns({
                  task: taskFilter || undefined,
                  before: nextBefore,
                  append: true,
                })
              }
            >
              加载更早运行记录
            </Button>
          </div>
        )}
      </Card>

      <Card>
        <CardHeader
          title="旧版 Windows 计划任务处置对照（只读）"
          subtitle="迁移至 Rust 监督进程后的历史任务状态"
        />
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-xs">
            <thead className="bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
              <tr>
                <th className="px-4 py-2 font-medium">旧任务名</th>
                <th className="px-3 py-2 font-medium">处置结论</th>
                <th className="px-3 py-2 font-medium">替代项</th>
                <th className="px-4 py-2 font-medium">说明</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--border-hairline)]">
              {legacyTasks.length === 0 ? (
                <tr>
                  <td colSpan={4} className="px-4 py-4 text-center text-[var(--text-muted)]">
                    暂无旧版计划任务清单
                  </td>
                </tr>
              ) : (
                legacyTasks.map((item) => (
                  <tr key={item.name}>
                    <td className="px-4 py-2 font-mono">{item.name}</td>
                    <td className="px-3 py-2">
                      <Badge tone={item.disposition === 'migrated' ? 'success' : 'neutral'}>
                        {item.disposition}
                      </Badge>
                    </td>
                    <td className="px-3 py-2 font-mono text-[11px]">
                      {item.replacement || '—'}
                    </td>
                    <td className="px-4 py-2 text-[var(--text-secondary)]">{item.note}</td>
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
