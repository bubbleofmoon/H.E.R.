import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Plus, RefreshCw } from 'lucide-react';
import {
  ApiRequestError,
  SettingItemView,
  fetchSettingsList,
  resetSettingValue,
  saveSettingValue,
} from '../api/client';
import {
  LegacyTaskRow,
  RunView,
  ScheduleEntry,
  ScheduleHistoryRow,
  TaskRow,
  beginScheduleWrite,
  bumpSettingsWriteGen,
  commitSharedSettingsRead,
  fetchSchedule,
  fetchScheduleHistory,
  fetchTaskRuns,
  fetchTasks,
  finishScheduleWrite,
  getSettingsWriteGen,
  getSharedScheduleState,
  hasActiveScheduleWrites,
  markSharedScheduleStale,
  markSharedSettingsReadFailed,
  refreshSharedScheduleSettings,
  runTaskNow,
  subscribeScheduleSync,
} from '../api/system';
import { Badge, Button, Card, PageHeader, Tabs } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';
import { RuntimeZoneInfo, formatErrorMessage } from '../utils/systemFormat';
import { TaskRunsTab } from './schedule/TaskRunsTab';
import { UpcomingScheduleTab } from './schedule/UpcomingScheduleTab';

function parseHhMmMinutes(val: unknown): number | null {
  if (typeof val !== 'string') return null;
  const m = /^([01]\d|2[0-3]):([0-5]\d)$/.exec(val.trim());
  if (!m) return null;
  return Number(m[1]) * 60 + Number(m[2]);
}

function formatHhMm(mins: number): string {
  const norm = ((mins % 1440) + 1440) % 1440;
  const h = Math.floor(norm / 60);
  const m = norm % 60;
  return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}`;
}

function circularGapMinutes(a: number, b: number): number {
  const d = Math.abs(a - b);
  return Math.min(d, 1440 - d);
}

function findSafeSlot(
  avoidMinutes: (number | null)[],
  excludedCheckMinutes: number[],
): string | null {
  const avoid = avoidMinutes.filter((v): v is number => typeof v === 'number');
  for (let m = 0; m < 1440; m += 1) {
    if (excludedCheckMinutes.includes(m % 30)) continue;
    if (avoid.every((other) => circularGapMinutes(m, other) >= 30)) {
      return formatHhMm(m);
    }
  }
  return null;
}

function canonicalSettingValue(val: unknown): unknown {
  return val === undefined || val === null ? '' : val;
}

function settingValuesEqual(a: unknown, b: unknown): boolean {
  return (
    JSON.stringify(canonicalSettingValue(a)) ===
    JSON.stringify(canonicalSettingValue(b))
  );
}

export const SchedulePage: React.FC = () => {
  const {
    activeTabs,
    setActiveTab,
    settings,
    conversations,
    refreshLiveData,
    openConfirm,
    pushToast,
    setDirtyEntry,
  } = useConsole();

  const tab = activeTabs.schedule === 'runs' ? 'runs' : 'upcoming';

  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [loadNotice, setLoadNotice] = useState<string | null>(null);
  const [entries, setEntries] = useState<ScheduleEntry[]>([]);
  const [timeZone, setTimeZone] = useState<RuntimeZoneInfo | null>(null);
  const [history, setHistory] = useState<ScheduleHistoryRow[]>([]);
  const [tasks, setTasks] = useState<TaskRow[]>([]);
  const [legacyTasks, setLegacyTasks] = useState<LegacyTaskRow[]>([]);
  const [createOpen, setCreateOpen] = useState(false);
  const createGenRef = useRef(0);

  const initialShared = getSharedScheduleState();
  const [dirtyDrafts, setDirtyDrafts] = useState<Record<string, unknown>>({});
  const [verifiedSettings, setVerifiedSettings] = useState<SettingItemView[] | null>(
    initialShared.settings,
  );
  const settingEditGenRef = useRef<Record<string, number>>({});
  const inFlightWritesRef = useRef(0);
  const [writeCycle, setWriteCycle] = useState(0);
  const [scheduleBaselineStale, setScheduleBaselineStale] = useState(initialShared.stale);

  const [taskFilter, setTaskFilter] = useState('');
  const taskFilterRef = useRef(taskFilter);
  taskFilterRef.current = taskFilter;
  const [runs, setRuns] = useState<RunView[]>([]);
  const [nextBefore, setNextBefore] = useState<number | null>(null);
  const [runsLoading, setRunsLoading] = useState(false);
  const [followedRun, setFollowedRun] = useState<{ runId: string; seq: number } | null>(null);
  const followSeqRef = useRef(0);

  const mountedRef = useRef(true);
  const activeLoadsRef = useRef(0);
  const loadSeqRef = useRef(0);
  const runsSeqRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    const syncFromShared = () => {
      if (!mountedRef.current) return;
      const snapshot = getSharedScheduleState();
      setVerifiedSettings(snapshot.settings);
      setScheduleBaselineStale(snapshot.stale);
      if (snapshot.activeWrites === 0) {
        setWriteCycle((c) => c + 1);
      }
    };
    const unsubscribe = subscribeScheduleSync(syncFromShared);
    syncFromShared();
    return () => {
      mountedRef.current = false;
      unsubscribe();
      setDirtyEntry(null);
    };
  }, [setDirtyEntry]);

  const scheduleSettings = useMemo(
    () => (verifiedSettings ?? settings).filter((s) => s.group === 'schedule'),
    [verifiedSettings, settings],
  );

  // Prune any idle drafts whose value matches current verified server state so stale keys cannot linger.
  useEffect(() => {
    const shared = getSharedScheduleState();
    if (
      inFlightWritesRef.current > 0 ||
      shared.activeWrites > 0 ||
      scheduleBaselineStale ||
      shared.stale
    ) {
      return;
    }
    setDirtyDrafts((prev) => {
      let changed = false;
      const next = { ...prev };
      for (const k of Object.keys(prev)) {
        const serverItem = scheduleSettings.find((s) => s.key === k);
        if (serverItem && settingValuesEqual(prev[k], serverItem.value)) {
          delete next[k];
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [scheduleBaselineStale, scheduleSettings, writeCycle]);

  const dirtySettings = useMemo(
    () =>
      scheduleSettings.filter(
        (s) =>
          Object.prototype.hasOwnProperty.call(dirtyDrafts, s.key) &&
          (scheduleBaselineStale || !settingValuesEqual(dirtyDrafts[s.key], s.value)),
      ),
    [dirtyDrafts, scheduleBaselineStale, scheduleSettings],
  );

  const loadUpcoming = useCallback(async () => {
    const seq = ++loadSeqRef.current;
    const readGen = getSettingsWriteGen();
    activeLoadsRef.current += 1;
    setLoading(true);
    try {
      const [schedRes, histRes, tasksRes, settingsRes] = await Promise.allSettled([
        fetchSchedule(),
        fetchScheduleHistory(100),
        fetchTasks(),
        fetchSettingsList(),
      ]);
      if (settingsRes.status === 'fulfilled') {
        const list = Array.isArray(settingsRes.value.settings) ? settingsRes.value.settings : [];
        commitSharedSettingsRead(list, readGen);
      } else {
        markSharedSettingsReadFailed(readGen);
      }

      if (!mountedRef.current || seq !== loadSeqRef.current) return;

      const warnings: string[] = [];
      if (schedRes.status === 'fulfilled') {
        setEntries(schedRes.value.entries || []);
        setTimeZone(schedRes.value.timeZone ?? null);
      } else {
        warnings.push(`日程列表 (${formatErrorMessage(schedRes.reason, '读取失败')})`);
      }
      if (histRes.status === 'fulfilled') {
        setHistory(histRes.value.history || []);
      } else {
        warnings.push(`提醒历史 (${formatErrorMessage(histRes.reason, '读取失败')})`);
      }
      if (tasksRes.status === 'fulfilled') {
        setTasks(tasksRes.value.tasks || []);
        setLegacyTasks(tasksRes.value.legacy || []);
      } else {
        warnings.push(`任务注册表 (${formatErrorMessage(tasksRes.reason, '读取失败')})`);
      }
      if (settingsRes.status === 'rejected') {
        warnings.push(`设置基准 (${formatErrorMessage(settingsRes.reason, '读取失败')})`);
      }
      setLoadNotice(warnings.length > 0 ? `部分数据读取失败：${warnings.join('；')}` : null);
    } finally {
      activeLoadsRef.current = Math.max(0, activeLoadsRef.current - 1);
      if (mountedRef.current) setLoading(activeLoadsRef.current > 0);
    }
  }, []);

  const loadRuns = useCallback(
    async (opts?: { task?: string; before?: number | null; append?: boolean }) => {
      const seq = ++runsSeqRef.current;
      const targetTask =
        opts && Object.prototype.hasOwnProperty.call(opts, 'task')
          ? opts.task
          : taskFilterRef.current || undefined;
      setRunsLoading(true);
      try {
        const res = await fetchTaskRuns({
          task: targetTask,
          before: opts?.before ?? null,
          limit: 50,
        });
        if (!mountedRef.current || seq !== runsSeqRef.current) return;
        setRuns((prev) => (opts?.append ? [...prev, ...(res.runs || [])] : res.runs || []));
        setNextBefore(res.nextBefore ?? null);
      } catch (err) {
        if (mountedRef.current && seq === runsSeqRef.current) {
          if (!opts?.append) {
            setRuns([]);
            setNextBefore(null);
          }
          pushToast(formatErrorMessage(err, '读取任务运行历史失败'), 'danger');
        }
      } finally {
        if (mountedRef.current && seq === runsSeqRef.current) {
          setRunsLoading(false);
        }
      }
    },
    [pushToast],
  );

  useEffect(() => {
    void loadUpcoming();
  }, [loadUpcoming]);

  useEffect(() => {
    if (tab === 'runs') {
      void loadRuns({ task: taskFilter || undefined, append: false });
    }
  }, [tab, taskFilter, loadRuns]);

  const handleSelectRunLog = useCallback((runId: string | null) => {
    if (!runId) {
      setFollowedRun(null);
      return;
    }
    const seq = ++followSeqRef.current;
    setFollowedRun({ runId, seq });
  }, []);

  const handleQuickRunTask = useCallback(
    (task: TaskRow) => {
      if (busy) return;
      openConfirm({
        title: `确认立即运行系统任务「${task.title}」？`,
        description: `${task.effect}（任务 ID: ${task.id}）`,
        confirmLabel: '立即运行',
        danger: false,
        onConfirm: async () => {
          setBusy(true);
          try {
            const res = await runTaskNow(task.id);
            pushToast(`任务 ${task.id} 已触发（${res.runId}）`, 'success');
            if (mountedRef.current) {
              if (res.runId) {
                const seq = ++followSeqRef.current;
                setFollowedRun({ runId: res.runId, seq });
              }
              await Promise.all([
                loadUpcoming(),
                loadRuns({ task: taskFilterRef.current || undefined, append: false }),
              ]);
            }
          } catch (err) {
            const conflictRunId =
              err instanceof ApiRequestError &&
              err.status === 409 &&
              typeof err.payload.runId === 'string'
                ? err.payload.runId
                : null;
            pushToast(
              formatErrorMessage(err, '触发任务失败'),
              conflictRunId ? 'warn' : 'danger',
            );
            if (mountedRef.current && conflictRunId) {
              const seq = ++followSeqRef.current;
              setFollowedRun({ runId: conflictRunId, seq });
              await loadRuns({ task: taskFilterRef.current || undefined, append: false });
            }
          } finally {
            if (mountedRef.current) setBusy(false);
          }
        },
      });
    },
    [busy, loadRuns, loadUpcoming, openConfirm, pushToast],
  );

  const handleDraftChange = useCallback(
    (key: string, val: unknown) => {
      settingEditGenRef.current[key] = (settingEditGenRef.current[key] ?? 0) + 1;
      const serverItem = scheduleSettings.find((s) => s.key === key);
      setDirtyDrafts((prev) => {
        const next = { ...prev };
        const shared = getSharedScheduleState();
        if (
          inFlightWritesRef.current === 0 &&
          shared.activeWrites === 0 &&
          !shared.stale &&
          serverItem &&
          settingValuesEqual(val, serverItem.value)
        ) {
          delete next[key];
        } else {
          next[key] = val;
        }
        return next;
      });
    },
    [scheduleSettings],
  );

  const verifyLiveScheduleBaseline = useCallback(async () => {
    bumpSettingsWriteGen();
    await refreshSharedScheduleSettings();
    await Promise.all([
      refreshLiveData(),
      mountedRef.current ? loadUpcoming() : Promise.resolve(),
    ]);
  }, [loadUpcoming, refreshLiveData]);

  const handleResetSetting = useCallback(
    (setting: SettingItemView) => {
      if (busy || inFlightWritesRef.current > 0 || hasActiveScheduleWrites()) {
        pushToast('当前有日程设置正在保存，请稍后再恢复默认', 'warn');
        return;
      }
      openConfirm({
        title: `恢复默认设置 ${setting.key}？`,
        description: `将清除自定义值并恢复为 ${JSON.stringify(setting.default)}。`,
        confirmLabel: '确认恢复',
        danger: Boolean(setting.confirm),
        onConfirm: async () => {
          if (!mountedRef.current || inFlightWritesRef.current > 0 || hasActiveScheduleWrites()) {
            pushToast('已有日程设置写入正在进行，请稍后重试', 'warn');
            return;
          }
          const startGen = settingEditGenRef.current[setting.key] ?? 0;
          inFlightWritesRef.current += 1;
          beginScheduleWrite();
          try {
            const res = await resetSettingValue(setting.key, Boolean(setting.confirm));
            if (typeof res.mirrorError === 'string' && res.mirrorError) {
              pushToast(
                `已恢复默认：${setting.key}，但写回配置文件失败：${res.mirrorError}`,
                'warn',
              );
            } else {
              pushToast(`已恢复默认：${setting.key}`, 'success');
            }
            if (mountedRef.current && (settingEditGenRef.current[setting.key] ?? 0) === startGen) {
              setDirtyDrafts((prev) => {
                if (!Object.prototype.hasOwnProperty.call(prev, setting.key)) return prev;
                const next = { ...prev };
                delete next[setting.key];
                return next;
              });
            }
            await verifyLiveScheduleBaseline();
          } catch (err) {
            pushToast(formatErrorMessage(err, '恢复默认设置失败'), 'danger');
            const isDefinitiveClientReject =
              err instanceof ApiRequestError && err.status >= 400 && err.status < 500;
            if (!isDefinitiveClientReject) {
              await verifyLiveScheduleBaseline();
            }
          } finally {
            inFlightWritesRef.current = Math.max(0, inFlightWritesRef.current - 1);
            finishScheduleWrite();
          }
        },
      });
    },
    [busy, openConfirm, pushToast, verifyLiveScheduleBaseline],
  );

  const handleSaveScheduleSettings = useCallback(async () => {
    if (
      busy ||
      inFlightWritesRef.current > 0 ||
      hasActiveScheduleWrites() ||
      dirtySettings.length === 0
    ) {
      return;
    }
    inFlightWritesRef.current += 1;
    beginScheduleWrite();
    setBusy(true);

    const errors: string[] = [];
    const mirrorWarnings: string[] = [];
    const savedKeys: { key: string; startGen: number }[] = [];
    let anyWriteDispatched = false;

    try {
      let baselineList = scheduleSettings;
      if (getSharedScheduleState().stale || dirtySettings.length >= 2) {
        const fresh = await refreshSharedScheduleSettings();
        if (!fresh) {
          markSharedScheduleStale();
          pushToast('无法读取最新服务端日程基准，已取消保存，请刷新后重试', 'danger');
          return;
        }
        baselineList = fresh.filter((s) => s.group === 'schedule');
      }

      const committedValues: Record<string, unknown> = {};
      for (const s of baselineList) {
        committedValues[s.key] = s.value;
      }

      const writeSingleSetting = async (setting: SettingItemView, value: unknown) => {
        const res = await saveSettingValue(setting.key, value, Boolean(setting.confirm));
        if (typeof res.mirrorError === 'string' && res.mirrorError) {
          mirrorWarnings.push(`${setting.key}: ${res.mirrorError}`);
        }
      };

      let pending = dirtySettings
        .map((s) => ({
          setting: s,
          value: dirtyDrafts[s.key],
          startGen: settingEditGenRef.current[s.key] ?? 0,
        }))
        .filter((item) => {
          if (item.value === null || item.value === undefined || item.value === '') {
            errors.push(`${item.setting.key} 不能为空`);
            return false;
          }
          return true;
        });

      while (pending.length > 0 && mountedRef.current) {
        let progress = false;
        const retryNext: typeof pending = [];
        for (const item of pending) {
          if (!mountedRef.current) break;
          anyWriteDispatched = true;
          try {
            await writeSingleSetting(item.setting, item.value);
            committedValues[item.setting.key] = item.value;
            savedKeys.push({ key: item.setting.key, startGen: item.startGen });
            progress = true;
          } catch (err) {
            if (err instanceof ApiRequestError && err.status === 409) {
              retryNext.push(item);
            } else {
              errors.push(`${item.setting.key}: ${formatErrorMessage(err, '保存失败')}`);
            }
          }
        }
        if (!mountedRef.current) break;
        if (progress) {
          pending = retryNext;
          continue;
        }

        // Resolve remaining 2-way or 3-way 409 conflicts using live committedValues.
        const digestItem = retryNext.find((i) => i.setting.key === 'memory.digest.time');
        const backupItem = retryNext.find((i) => i.setting.key === 'backup.time');
        const checkMinItem = retryNext.find((i) => i.setting.key === 'memory.digest.checkMinute');

        const storedDigestVal = committedValues['memory.digest.time'];
        const storedBackupVal = committedValues['backup.time'];
        const storedCheckVal = committedValues['memory.digest.checkMinute'];

        const excludedCheckMinutes = [
          storedCheckVal,
          checkMinItem?.value,
        ].filter((v): v is number => typeof v === 'number' && Number.isInteger(v));
        const effectiveCheckMin =
          typeof (checkMinItem ? checkMinItem.value : storedCheckVal) === 'number' &&
          Number.isInteger(checkMinItem ? checkMinItem.value : storedCheckVal)
            ? ((checkMinItem ? checkMinItem.value : storedCheckVal) as number)
            : null;

        const storedDigestMins = parseHhMmMinutes(storedDigestVal);
        const storedBackupMins = parseHhMmMinutes(storedBackupVal);
        const targetDigestMins = digestItem
          ? parseHhMmMinutes(digestItem.value)
          : storedDigestMins;
        const targetBackupMins = backupItem
          ? parseHhMmMinutes(backupItem.value)
          : storedBackupMins;

        const targetGapValid =
          targetDigestMins !== null &&
          targetBackupMins !== null &&
          circularGapMinutes(targetDigestMins, targetBackupMins) >= 30 &&
          (effectiveCheckMin === null ||
            (targetDigestMins % 30 !== effectiveCheckMin &&
              targetBackupMins % 30 !== effectiveCheckMin));

        const conflictCount = [digestItem, backupItem, checkMinItem].filter(Boolean).length;
        if (conflictCount >= 2 && targetGapValid) {
          const tempDigest = digestItem
            ? findSafeSlot([storedBackupMins, targetBackupMins], excludedCheckMinutes)
            : null;
          const tempDigestMins = tempDigest ? parseHhMmMinutes(tempDigest) : storedDigestMins;
          const tempBackup =
            backupItem && checkMinItem
              ? findSafeSlot([tempDigestMins, targetDigestMins], excludedCheckMinutes)
              : null;

          if ((!digestItem || tempDigest) && (!(backupItem && checkMinItem) || tempBackup)) {
            let attemptedDigestPark = false;
            let attemptedBackupPark = false;
            let attemptedCheck = false;
            let attemptedBackupTarget = false;
            let attemptedDigestTarget = false;
            try {
              if (digestItem && tempDigest) {
                anyWriteDispatched = true;
                attemptedDigestPark = true;
                await writeSingleSetting(digestItem.setting, tempDigest);
              }
              if (backupItem && checkMinItem && tempBackup) {
                anyWriteDispatched = true;
                attemptedBackupPark = true;
                await writeSingleSetting(backupItem.setting, tempBackup);
              }
              if (checkMinItem) {
                anyWriteDispatched = true;
                attemptedCheck = true;
                await writeSingleSetting(checkMinItem.setting, checkMinItem.value);
              }
              if (backupItem) {
                anyWriteDispatched = true;
                attemptedBackupTarget = true;
                await writeSingleSetting(backupItem.setting, backupItem.value);
              }
              if (digestItem) {
                anyWriteDispatched = true;
                attemptedDigestTarget = true;
                await writeSingleSetting(digestItem.setting, digestItem.value);
              }

              if (backupItem) {
                committedValues[backupItem.setting.key] = backupItem.value;
                savedKeys.push({ key: backupItem.setting.key, startGen: backupItem.startGen });
              }
              if (digestItem) {
                committedValues[digestItem.setting.key] = digestItem.value;
                savedKeys.push({ key: digestItem.setting.key, startGen: digestItem.startGen });
              }
              if (checkMinItem) {
                committedValues[checkMinItem.setting.key] = checkMinItem.value;
                savedKeys.push({
                  key: checkMinItem.setting.key,
                  startGen: checkMinItem.startGen,
                });
              }
              pending = retryNext.filter(
                (i) =>
                  i.setting.key !== 'memory.digest.time' &&
                  i.setting.key !== 'backup.time' &&
                  i.setting.key !== 'memory.digest.checkMinute',
              );
              if (pending.length > 0) continue;
              break;
            } catch (swapErr) {
              // Reconcile uncertain/lost-response writes against server only for keys this swap actually touched.
              const latest = await fetchSettingsList().catch(() => null);
              if (!latest) {
                errors.push(
                  `日程时间协同保存失败：${formatErrorMessage(swapErr, 'HTTP 409')}；无法重新读取服务端设置，未自动回滚（请刷新页面确认临时时间是否仍生效）`,
                );
                break;
              }
              const serverValue = (key: string) =>
                latest.settings.find((s) => s.key === key)?.value;
              const digestServerVal = digestItem ? serverValue(digestItem.setting.key) : undefined;
              const backupServerVal = backupItem ? serverValue(backupItem.setting.key) : undefined;
              const checkServerVal = checkMinItem ? serverValue(checkMinItem.setting.key) : undefined;

              const digestNeedsRevert = Boolean(
                digestItem &&
                  (attemptedDigestPark || attemptedDigestTarget) &&
                  ((tempDigest !== null && settingValuesEqual(digestServerVal, tempDigest)) ||
                    (attemptedDigestTarget &&
                      settingValuesEqual(digestServerVal, digestItem.value))),
              );
              const checkNeedsRevert = Boolean(
                checkMinItem &&
                  attemptedCheck &&
                  settingValuesEqual(checkServerVal, checkMinItem.value),
              );
              const backupAtTemp = Boolean(
                backupItem &&
                  attemptedBackupPark &&
                  tempBackup &&
                  settingValuesEqual(backupServerVal, tempBackup),
              );
              const backupNeedsRevert = Boolean(
                backupItem &&
                  (attemptedBackupPark || attemptedBackupTarget) &&
                  (backupAtTemp ||
                    (attemptedBackupTarget &&
                      settingValuesEqual(backupServerVal, backupItem.value))),
              );

              const revertFailures: string[] = [];
              const tryRevert = async (setting: SettingItemView | undefined, value: unknown) => {
                if (!setting) return;
                try {
                  const revertRes = await saveSettingValue(
                    setting.key,
                    value,
                    Boolean(setting.confirm),
                  );
                  if (typeof revertRes.mirrorError === 'string' && revertRes.mirrorError) {
                    mirrorWarnings.push(`${setting.key} (回滚): ${revertRes.mirrorError}`);
                  }
                } catch {
                  revertFailures.push(`${setting.key} -> ${String(value)}`);
                }
              };

              if (
                digestItem &&
                digestNeedsRevert &&
                tempDigest &&
                (backupNeedsRevert || checkNeedsRevert) &&
                !settingValuesEqual(digestServerVal, tempDigest)
              ) {
                await tryRevert(digestItem.setting, tempDigest);
              }
              if (backupNeedsRevert && !backupAtTemp && tempBackup && checkNeedsRevert) {
                await tryRevert(backupItem?.setting, tempBackup);
              }
              if (checkNeedsRevert) {
                await tryRevert(checkMinItem?.setting, storedCheckVal);
              }
              if (backupNeedsRevert) {
                await tryRevert(backupItem?.setting, storedBackupVal);
              }
              if (digestNeedsRevert) {
                await tryRevert(digestItem?.setting, storedDigestVal);
              }

              const revertNote =
                revertFailures.length > 0
                  ? `（以下回滚失败，临时时间可能仍生效：${revertFailures.join('、')}）`
                  : '';
              errors.push(
                `日程时间协同保存失败：${formatErrorMessage(swapErr, 'HTTP 409')}${revertNote}`,
              );
              break;
            }
          }
        }

        for (const item of retryNext) {
          errors.push(`${item.setting.key}: 日程时间冲突（HTTP 409）`);
        }
        break;
      }

      if (anyWriteDispatched) {
        await verifyLiveScheduleBaseline();
        if (mountedRef.current) {
          setDirtyDrafts((prev) => {
            const next = { ...prev };
            for (const { key, startGen } of savedKeys) {
              if ((settingEditGenRef.current[key] ?? 0) === startGen) {
                delete next[key];
              }
            }
            return next;
          });
        }
      }

      if (errors.length > 0) {
        pushToast(`部分日程设置保存失败：${errors.join('；')}`, 'danger');
      } else if (savedKeys.length > 0) {
        pushToast(`已保存 ${savedKeys.length} 项日程设置`, 'success');
      }
      if (mirrorWarnings.length > 0) {
        pushToast(
          `部分日程设置已写入，但写回配置文件失败：${mirrorWarnings.join('；')}`,
          'warn',
        );
      }
    } finally {
      inFlightWritesRef.current = Math.max(0, inFlightWritesRef.current - 1);
      finishScheduleWrite();
      if (mountedRef.current) {
        setBusy(false);
      }
    }
  }, [busy, dirtyDrafts, dirtySettings, pushToast, scheduleSettings, verifyLiveScheduleBaseline]);

  const handleDiscardScheduleSettings = useCallback(() => {
    for (const s of dirtySettings) {
      settingEditGenRef.current[s.key] = (settingEditGenRef.current[s.key] ?? 0) + 1;
    }
    setDirtyDrafts({});
    pushToast('已放弃未保存的日程设置改动', 'info');
  }, [dirtySettings, pushToast]);

  useEffect(() => {
    if (tab !== 'upcoming' || dirtySettings.length === 0) {
      setDirtyEntry(null);
      return;
    }
    setDirtyEntry({
      count: dirtySettings.length,
      label: `日程设置：${dirtySettings.length} 项未保存更改`,
      onSave: handleSaveScheduleSettings,
      onDiscard: handleDiscardScheduleSettings,
    });
  }, [
    tab,
    dirtySettings.length,
    handleSaveScheduleSettings,
    handleDiscardScheduleSettings,
    setDirtyEntry,
  ]);

  return (
    <div className="space-y-4">
      <PageHeader
        title="日程与任务"
        description="统一查看与控制系统定时任务、管理员提醒、会话潜水/冒泡计时器及后台任务运行日志。"
        badge={<Badge tone="neutral">时区：{timeZone?.label || '—'}</Badge>}
        actions={
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="secondary"
              disabled={loading || runsLoading}
              onClick={() => {
                void Promise.all([loadUpcoming(), refreshLiveData()]);
                if (tab === 'runs') void loadRuns({ task: taskFilter || undefined });
              }}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              刷新
            </Button>
            {tab === 'upcoming' && (
              <Button
                size="sm"
                variant="primary"
                onClick={() => {
                  createGenRef.current += 1;
                  setCreateOpen((v) => !v);
                }}
              >
                <Plus className="h-3.5 w-3.5" />
                新建提醒
              </Button>
            )}
          </div>
        }
      />

      <Tabs
        tabs={[
          { id: 'upcoming', label: '即将执行与提醒' },
          { id: 'runs', label: '任务运行历史' },
        ]}
        active={tab}
        onChange={(id) => setActiveTab('schedule', id)}
      />

      {loadNotice && (
        <Card className="px-4 py-2.5 text-xs text-[var(--warn)]">{loadNotice}</Card>
      )}

      {tab === 'upcoming' ? (
        <UpcomingScheduleTab
          loading={loading}
          busy={busy}
          setBusy={setBusy}
          entries={entries}
          history={history}
          timeZone={timeZone}
          conversations={conversations}
          scheduleSettings={scheduleSettings}
          dirtyDrafts={dirtyDrafts}
          onDraftChange={handleDraftChange}
          onResetSetting={handleResetSetting}
          createOpen={createOpen}
          setCreateOpen={setCreateOpen}
          createGenRef={createGenRef}
          mountedRef={mountedRef}
          onRefreshUpcoming={loadUpcoming}
        />
      ) : (
        <TaskRunsTab
          busy={busy}
          tasks={tasks}
          legacyTasks={legacyTasks}
          taskFilter={taskFilter}
          setTaskFilter={setTaskFilter}
          runs={runs}
          runsLoading={runsLoading}
          nextBefore={nextBefore}
          timeZone={timeZone}
          followedRun={followedRun}
          onSelectRunLog={handleSelectRunLog}
          onQuickRunTask={handleQuickRunTask}
          taskFilterRef={taskFilterRef}
          onLoadRuns={loadRuns}
          onRefreshUpcoming={loadUpcoming}
        />
      )}
    </div>
  );
};
