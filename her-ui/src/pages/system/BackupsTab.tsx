import React from 'react';
import { ArchiveRestore, HardDriveDownload } from 'lucide-react';
import { ApiRequestError } from '../../api/client';
import {
  BackupArchiveItem,
  BackupSettings,
  RestorePreview,
  RunView,
  beginBackupSave,
  bumpSettingsWriteGen,
  executeBackupRestore,
  finishBackupSave,
  hasActiveBackupSaves,
  markSharedScheduleStale,
  previewBackupRestore,
  refreshSharedScheduleSettings,
  runTaskNow,
  saveBackupSettings,
} from '../../api/system';
import { Badge, Button, Card, CardHeader, Switch } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatBytes, formatErrorMessage, formatRuntimeTime } from '../../utils/systemFormat';

export interface BackupSettingsDirtyPatch {
  dir?: string;
  keepDays?: string;
  time?: string;
  includeState?: boolean;
}

interface BackupsTabProps {
  mountId: number;
  busy: boolean;
  backupSaveActive: boolean;
  setBusy: React.Dispatch<React.SetStateAction<boolean>>;
  backups: BackupArchiveItem[];
  backupsStale: boolean;
  setBackupsStale: React.Dispatch<React.SetStateAction<boolean>>;
  failedBackups: RunView[];
  backupSettings: BackupSettings | null;
  backupSettingsStale: boolean;
  setBackupSettings: React.Dispatch<React.SetStateAction<BackupSettings | null>>;
  setBackupSettingsStale: React.Dispatch<React.SetStateAction<boolean>>;
  backupDirty: BackupSettingsDirtyPatch;
  setBackupDirty: React.Dispatch<React.SetStateAction<BackupSettingsDirtyPatch>>;
  backupFieldGenRef: React.MutableRefObject<Partial<Record<keyof BackupSettingsDirtyPatch, number>>>;
  savingBackupRef: React.MutableRefObject<boolean>;
  backupSetGenRef: React.MutableRefObject<number>;
  restoreMemoryOnly: boolean;
  setRestoreMemoryOnly: React.Dispatch<React.SetStateAction<boolean>>;
  restoreTarget: { archiveName: string; preview: RestorePreview } | null;
  setRestoreTarget: React.Dispatch<
    React.SetStateAction<{ archiveName: string; preview: RestorePreview } | null>
  >;
  previewSeqRef: React.MutableRefObject<number>;
  mountedRef: React.MutableRefObject<boolean>;
  onRefreshAll: () => Promise<void>;
}

function isFieldReverted(
  field: keyof BackupSettingsDirtyPatch,
  value: unknown,
  baseline: BackupSettings,
): boolean {
  if (field === 'dir' && typeof value === 'string') {
    return value.trim() === (baseline.dir || '');
  }
  if (field === 'keepDays' && typeof value === 'string') {
    return value.trim() === String(baseline.keepDays);
  }
  if (field === 'time' && typeof value === 'string') {
    return value.trim() === (baseline.time || '');
  }
  if (field === 'includeState' && typeof value === 'boolean') {
    return value === Boolean(baseline.includeState);
  }
  return false;
}

export function pruneRevertedBackupDirty(
  prev: BackupSettingsDirtyPatch,
  baseline: BackupSettings,
): BackupSettingsDirtyPatch {
  let changed = false;
  const next: BackupSettingsDirtyPatch = { ...prev };
  for (const k of Object.keys(next) as (keyof BackupSettingsDirtyPatch)[]) {
    if (isFieldReverted(k, next[k], baseline)) {
      delete next[k];
      changed = true;
    }
  }
  return changed ? next : prev;
}

export const BackupsTab: React.FC<BackupsTabProps> = ({
  mountId,
  busy,
  backupSaveActive,
  setBusy,
  backups,
  backupsStale,
  setBackupsStale,
  failedBackups,
  backupSettings,
  backupSettingsStale,
  setBackupSettings,
  setBackupSettingsStale,
  backupDirty,
  setBackupDirty,
  backupFieldGenRef,
  savingBackupRef,
  backupSetGenRef,
  restoreMemoryOnly,
  setRestoreMemoryOnly,
  restoreTarget,
  setRestoreTarget,
  previewSeqRef,
  mountedRef,
  onRefreshAll,
}) => {
  const { openConfirm, pushToast, refreshLiveData } = useConsole();

  const settingsReady = backupSettings !== null;
  const effectiveDir = backupDirty.dir !== undefined ? backupDirty.dir : backupSettings?.dir || '';
  const effectiveKeepDays =
    backupDirty.keepDays !== undefined
      ? backupDirty.keepDays
      : backupSettings
        ? String(backupSettings.keepDays)
        : '';
  const effectiveTime =
    backupDirty.time !== undefined
      ? backupDirty.time
      : backupSettings?.time || '';
  const effectiveIncludeState =
    backupDirty.includeState !== undefined
      ? backupDirty.includeState
      : Boolean(backupSettings?.includeState);

  const hasDirtyFields = Object.keys(backupDirty).length > 0;

  const updateBackupField = (
    field: keyof BackupSettingsDirtyPatch,
    patch: BackupSettingsDirtyPatch,
  ) => {
    if (!settingsReady || !backupSettings) return;
    backupFieldGenRef.current[field] = (backupFieldGenRef.current[field] ?? 0) + 1;
    setBackupDirty((prev) => {
      const next: BackupSettingsDirtyPatch = { ...prev, ...patch };
      return savingBackupRef.current || backupSettingsStale
        ? next
        : pruneRevertedBackupDirty(next, backupSettings);
    });
  };

  const handleSaveBackupSettings = async () => {
    if (busy || hasActiveBackupSaves() || !settingsReady || !hasDirtyFields) return;
    const payload: {
      dir?: string | null;
      keepDays?: number;
      time?: string | null;
      includeState?: boolean;
    } = {};

    const submittedGens: Partial<Record<keyof BackupSettingsDirtyPatch, number>> = {};

    if (backupDirty.dir !== undefined) {
      payload.dir = backupDirty.dir.trim() ? backupDirty.dir.trim() : null;
      submittedGens.dir = backupFieldGenRef.current.dir ?? 0;
    }
    if (backupDirty.keepDays !== undefined) {
      const keepDays = Number(backupDirty.keepDays.trim());
      if (!Number.isInteger(keepDays) || keepDays < 1) {
        pushToast('保留天数需为大于等于 1 的整数', 'warn');
        return;
      }
      payload.keepDays = keepDays;
      submittedGens.keepDays = backupFieldGenRef.current.keepDays ?? 0;
    }
    if (backupDirty.time !== undefined) {
      const timeVal = backupDirty.time.trim();
      if (timeVal && !/^([01]\d|2[0-3]):[0-5]\d$/.test(timeVal)) {
        pushToast('备份时间需为 HH:MM 格式（例如 04:00）', 'warn');
        return;
      }
      payload.time = timeVal || null;
      submittedGens.time = backupFieldGenRef.current.time ?? 0;
    }
    if (backupDirty.includeState !== undefined) {
      payload.includeState = backupDirty.includeState;
      submittedGens.includeState = backupFieldGenRef.current.includeState ?? 0;
    }

    const hasDirChange = payload.dir !== undefined;
    const hasTimeChange = payload.time !== undefined;
    savingBackupRef.current = true;
    beginBackupSave(hasDirChange);
    if (hasTimeChange) {
      bumpSettingsWriteGen();
      markSharedScheduleStale();
    }
    setBusy(true);
    let finalized = false;
    const finalizeSave = () => {
      if (!finalized) {
        finalized = true;
        finishBackupSave(hasDirChange, mountId);
      }
    };

    try {
      const res = await saveBackupSettings(payload);
      backupSetGenRef.current += 1;
      savingBackupRef.current = false;
      finalizeSave();
      if (hasTimeChange) {
        bumpSettingsWriteGen();
        void refreshSharedScheduleSettings();
      }
      if (mountedRef.current) {
        setBackupSettings(res.settings);
        setBackupSettingsStale(false);
        setBackupDirty((prev) => {
          const next = { ...prev };
          for (const [k, sGen] of Object.entries(submittedGens) as [
            keyof BackupSettingsDirtyPatch,
            number,
          ][]) {
            if ((backupFieldGenRef.current[k] ?? 0) === sGen) {
              delete next[k];
            }
          }
          // Prune any other fields reverted back to latest server baseline while save was pending.
          return pruneRevertedBackupDirty(next, res.settings);
        });
        if (hasDirChange) {
          previewSeqRef.current += 1;
          setRestoreTarget(null);
          setBackupsStale(true);
        }
      }
      pushToast('已保存备份策略设置', 'success');
      await Promise.all([
        mountedRef.current ? onRefreshAll() : Promise.resolve(),
        refreshLiveData(),
      ]);
    } catch (err) {
      savingBackupRef.current = false;
      finalizeSave();
      pushToast(formatErrorMessage(err, '保存备份策略失败'), 'danger');
      const isDefinitiveClientReject =
        err instanceof ApiRequestError && err.status >= 400 && err.status < 500;
      if (!isDefinitiveClientReject) {
        if (hasTimeChange) {
          bumpSettingsWriteGen();
          void refreshSharedScheduleSettings();
        }
        if (mountedRef.current) {
          setBackupSettingsStale(true);
          if (hasDirChange) {
            previewSeqRef.current += 1;
            setRestoreTarget(null);
            setBackupsStale(true);
          }
          await onRefreshAll();
        }
      }
    } finally {
      savingBackupRef.current = false;
      finalizeSave();
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleRunBackupNow = () => {
    if (busy) return;
    openConfirm({
      title: '确认立即执行数据备份？',
      description: '将排队执行 backup 任务并生成新的备份归档快照。',
      confirmLabel: '立即备份',
      danger: false,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await runTaskNow('backup');
          pushToast(`已排队立即备份（${res.runId}）`, 'success');
          if (mountedRef.current) await onRefreshAll();
        } catch (err) {
          pushToast(formatErrorMessage(err, '触发备份任务失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const invalidatePreview = () => {
    previewSeqRef.current += 1;
    setRestoreTarget(null);
  };

  const handlePreviewRestore = async (archiveName: string) => {
    if (busy) return;
    const seq = ++previewSeqRef.current;
    const memorySnapshot = restoreMemoryOnly;
    setRestoreTarget(null);
    setBusy(true);
    try {
      const res = await previewBackupRestore(archiveName, memorySnapshot);
      if (!mountedRef.current || previewSeqRef.current !== seq) return;
      setRestoreTarget({ archiveName, preview: res.preview });
      pushToast(`已生成 ${archiveName} 还原预览，请核对告警后确认`, 'info');
    } catch (err) {
      if (mountedRef.current && previewSeqRef.current === seq) {
        setRestoreTarget(null);
      }
      pushToast(formatErrorMessage(err, '生成还原预览失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleExecuteRestore = () => {
    if (!restoreTarget || busy) return;
    const { archiveName, preview } = restoreTarget;
    const memoryOnly = preview.scope === 'memory';
    const submittedSeq = previewSeqRef.current;
    openConfirm({
      title: `确认从归档 ${archiveName} 执行${memoryOnly ? '仅记忆' : '完整'}还原？`,
      description: memoryOnly
        ? '仅覆盖记忆数据并写入审计记录，随后重启应用记忆状态。'
        : '完整还原将覆盖或清除管理员密码与端点 API 密钥，控制台会立即重启且所有浏览器会话将注销重登！',
      confirmLabel: '确认执行还原',
      danger: true,
      onConfirm: async () => {
        if (!mountedRef.current || previewSeqRef.current !== submittedSeq) {
          pushToast('还原预览已变更或失效，请重新生成预览后再执行还原', 'warn');
          return;
        }
        setBusy(true);
        try {
          const res = await executeBackupRestore(
            archiveName,
            memoryOnly,
            preview.confirmation,
          );
          if (mountedRef.current) {
            previewSeqRef.current += 1;
            setRestoreTarget(null);
          }
          pushToast(
            res.message || '还原任务已进入队列（HTTP 202），控制台即将重启',
            'success',
          );
        } catch (err) {
          pushToast(formatErrorMessage(err, '执行还原失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-3">
        <Card>
          <CardHeader
            title="备份策略设置"
            subtitle="自动每日快照路径、保留天数与状态范围"
            actions={
              <Button
                size="sm"
                variant="secondary"
                disabled={busy}
                onClick={handleRunBackupNow}
              >
                <HardDriveDownload className="h-3.5 w-3.5" />
                立即备份
              </Button>
            }
          />
          <div className="space-y-3 p-4 text-xs">
            <label className="block space-y-1">
              <span className="text-[var(--text-secondary)]">自定义归档目录（留空使用默认）</span>
              <input
                type="text"
                disabled={!settingsReady}
                value={effectiveDir}
                onChange={(e) => updateBackupField('dir', { dir: e.target.value })}
                placeholder={backupSettings?.resolvedDir || '读取默认运行时备份目录…'}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs disabled:opacity-50"
              />
            </label>
            <div className="grid grid-cols-2 gap-2">
              <label className="space-y-1">
                <span className="text-[var(--text-secondary)]">保留天数</span>
                <input
                  type="number"
                  min={1}
                  disabled={!settingsReady}
                  value={effectiveKeepDays}
                  onChange={(e) => updateBackupField('keepDays', { keepDays: e.target.value })}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs disabled:opacity-50"
                />
              </label>
              <label className="space-y-1">
                <span className="text-[var(--text-secondary)]">每日时间 (HH:MM)</span>
                <input
                  type="time"
                  disabled={!settingsReady}
                  value={effectiveTime}
                  onChange={(e) => updateBackupField('time', { time: e.target.value })}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs disabled:opacity-50"
                />
              </label>
            </div>
            <label className="flex items-center justify-between rounded border border-[var(--border-hairline)] px-3 py-2">
              <span>包含运行状态文件（完整快照）</span>
              <Switch
                disabled={!settingsReady}
                checked={effectiveIncludeState}
                onChange={(next) => updateBackupField('includeState', { includeState: next })}
                label="包含运行状态文件"
              />
            </label>
            {backupSettings?.resolvedDir && (
              <div className="text-[11px] text-[var(--text-muted)] break-all">
                实际目录：<code>{backupSettings.resolvedDir}</code>
              </div>
            )}
            <Button
              size="sm"
              variant="primary"
              disabled={busy || backupSaveActive || !settingsReady || !hasDirtyFields}
              onClick={() => void handleSaveBackupSettings()}
              className="w-full"
            >
              保存备份设置
            </Button>
          </div>
        </Card>

        <Card className="lg:col-span-2">
          <CardHeader
            title="备份归档清单"
            subtitle="选择归档进行还原预览；勾选「仅还原记忆」可避免覆盖系统配置与凭据"
            actions={
              <label className="inline-flex items-center gap-2 text-xs">
                <span>仅还原记忆</span>
                <Switch
                  checked={restoreMemoryOnly}
                  onChange={(next) => {
                    setRestoreMemoryOnly(next);
                    invalidatePreview();
                  }}
                  label="仅还原记忆"
                />
              </label>
            }
          />
          {backupsStale && (
            <div className="border-b border-amber-300 bg-[var(--warn-soft)] px-4 py-2 text-[11px] text-[var(--warn)] dark:border-amber-900">
              备份列表刷新失败（可能与当前备份目录不一致），已禁用预览操作，请先点击右上角「刷新」。
            </div>
          )}
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-xs">
              <thead className="bg-[var(--bg-subtle)] text-[var(--text-secondary)]">
                <tr>
                  <th className="px-4 py-2 font-medium">归档文件</th>
                  <th className="px-3 py-2 font-medium">时间</th>
                  <th className="px-3 py-2 font-medium">大小</th>
                  <th className="px-3 py-2 font-medium">校验状态</th>
                  <th className="px-4 py-2 font-medium text-right">还原</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--border-hairline)]">
                {backups.length === 0 ? (
                  <tr>
                    <td colSpan={5} className="px-4 py-6 text-center text-[var(--text-muted)]">
                      暂无备份归档
                    </td>
                  </tr>
                ) : (
                  backups.map((b) => (
                    <tr key={b.name}>
                      <td className="px-4 py-2 font-mono">{b.name}</td>
                      <td className="px-3 py-2 tabular-nums text-[11px]">
                        {formatRuntimeTime(b.time)}
                      </td>
                      <td className="px-3 py-2 tabular-nums">{formatBytes(b.size)}</td>
                      <td className="px-3 py-2">
                        <Badge tone={b.result === 'verified' ? 'success' : 'warn'}>
                          {b.result}
                        </Badge>
                      </td>
                      <td className="px-4 py-2 text-right">
                        <Button
                          size="sm"
                          variant="secondary"
                          disabled={busy || backupsStale}
                          onClick={() => void handlePreviewRestore(b.name)}
                        >
                          <ArchiveRestore className="h-3.5 w-3.5" />
                          预览还原
                        </Button>
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
          {failedBackups.length > 0 && (
            <div className="border-t border-[var(--border-hairline)] px-4 py-2.5 text-[11px] text-[var(--warn)]">
              最近存在 {failedBackups.length} 次失败/取消的备份尝试（最近一次：{failedBackups[0].runId}
              ）。
            </div>
          )}
        </Card>
      </div>

      {restoreTarget && (
        <Card className="border-amber-300 bg-[var(--warn-soft)]/40 p-4 space-y-3 text-xs dark:border-amber-900">
          <div className="flex items-center justify-between gap-2">
            <div className="font-semibold text-[var(--warn)]">
              还原预览：{restoreTarget.archiveName}（范围：{restoreTarget.preview.scope}）
            </div>
            <Button size="sm" variant="ghost" onClick={invalidatePreview}>
              关闭预览
            </Button>
          </div>
          <div className="rounded border border-amber-300 bg-[var(--bg-surface)] p-3 space-y-1.5 dark:border-amber-900">
            <div className="font-medium text-[var(--text-primary)]">安全与登录态影响提示（D2）</div>
            <p className="text-[var(--text-secondary)]">
              {restoreTarget.preview.scope === 'memory'
                ? '当前为仅还原记忆模式：管理员密码、控制台令牌与配置文件均不会被替换。'
                : '完整快照还原可能会覆盖或清除管理员密码与端点 API 密钥；还原启动后所有活动浏览器会话都会退出，重启完成需使用快照中的凭据或初始化流程重新登录。'}
            </p>
            {restoreTarget.preview.warnings.map((w, idx) => (
              <div key={idx} className="text-[var(--danger)]">
                • {w}
              </div>
            ))}
          </div>
          <div className="grid gap-2 sm:grid-cols-3 text-[11px]">
            <div>
              <span className="font-medium">将包含：</span>
              {restoreTarget.preview.contents.join(', ') || '—'}
            </div>
            <div>
              <span className="font-medium">将替换：</span>
              {restoreTarget.preview.replaces.join(', ') || '—'}
            </div>
            <div>
              <span className="font-medium">将移除：</span>
              {restoreTarget.preview.removes.join(', ') || '无'}
            </div>
          </div>
          <div className="flex justify-end">
            <Button size="sm" variant="danger" disabled={busy} onClick={handleExecuteRestore}>
              确认执行还原
            </Button>
          </div>
        </Card>
      )}
    </div>
  );
};
