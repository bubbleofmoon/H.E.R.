import React, { useCallback, useEffect, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import {
  BackupArchiveItem,
  BackupSettings,
  DependenciesResponse,
  DesiredView,
  HealthResponse,
  QqLoginResponse,
  ReleasesStatusResponse,
  RestorePreview,
  RunView,
  fetchBackupSettings,
  fetchBackups,
  fetchDependencies,
  fetchHealth,
  fetchQqLogin,
  fetchReleases,
  fetchTasks,
  getBackupDirVersion,
  getBackupSaveVersion,
  getReleaseSaveVersion,
  hasActiveBackupSaves,
  hasActiveReleaseSaves,
  hasPendingBackupDirSaves,
  nextSystemMountId,
  subscribeSystemSync,
} from '../api/system';
import { Badge, Button, Card, PageHeader, Tabs } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';
import { formatErrorMessage } from '../utils/systemFormat';
import {
  BackupSettingsDirtyPatch,
  BackupsTab,
  pruneRevertedBackupDirty,
} from './system/BackupsTab';
import { RuntimeControlTab } from './system/RuntimeControlTab';
import {
  ReleaseDirtyPatch,
  UpdatesTab,
  pruneRevertedReleaseDirty,
} from './system/UpdatesTab';

export const SystemPage: React.FC = () => {
  const { activeTabs, setActiveTab, qqLoginState, refreshLiveData } = useConsole();

  const tab =
    activeTabs.system === 'backups' || activeTabs.system === 'updates'
      ? activeTabs.system
      : 'runtime';

  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [loadNotice, setLoadNotice] = useState<string | null>(null);
  const [unsupportedNotice, setUnsupportedNotice] = useState<string | null>(null);

  const [desired, setDesired] = useState<DesiredView | null>(null);
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [qqLogin, setQqLogin] = useState<QqLoginResponse | null>(null);

  const [backups, setBackups] = useState<BackupArchiveItem[]>([]);
  const [backupsStale, setBackupsStale] = useState(() => hasPendingBackupDirSaves());
  const [failedBackups, setFailedBackups] = useState<RunView[]>([]);
  const [backupSettings, setBackupSettings] = useState<BackupSettings | null>(null);
  const [backupSettingsStale, setBackupSettingsStale] = useState(() => hasActiveBackupSaves());
  const [backupSaveActive, setBackupSaveActive] = useState(() => hasActiveBackupSaves());
  const [backupDirty, setBackupDirty] = useState<BackupSettingsDirtyPatch>({});
  const [restoreMemoryOnly, setRestoreMemoryOnly] = useState(false);
  const [restoreTarget, setRestoreTarget] = useState<{
    archiveName: string;
    preview: RestorePreview;
  } | null>(null);

  const [releases, setReleases] = useState<ReleasesStatusResponse | null>(null);
  const [releasesStale, setReleasesStale] = useState(() => hasActiveReleaseSaves());
  const [releaseSaveActive, setReleaseSaveActive] = useState(() => hasActiveReleaseSaves());
  const [releasesLoading, setReleasesLoading] = useState(false);
  const [releaseDirty, setReleaseDirty] = useState<ReleaseDirtyPatch>({});
  const [deps, setDeps] = useState<DependenciesResponse | null>(null);

  const mountIdRef = useRef<number>(nextSystemMountId());
  const mountedRef = useRef(true);
  const activeLoadsRef = useRef(0);
  const loadSeqRef = useRef(0);
  const previewSeqRef = useRef(0);
  const backupFieldGenRef = useRef<Partial<Record<keyof BackupSettingsDirtyPatch, number>>>({});
  const releaseFieldGenRef = useRef<Partial<Record<keyof ReleaseDirtyPatch, number>>>({});
  const savingBackupRef = useRef(false);
  const savingReleaseRef = useRef(false);
  const backupSetGenRef = useRef(0);
  const releasesGenRef = useRef(0);
  const depsGenRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    const previewSeq = previewSeqRef;
    return () => {
      mountedRef.current = false;
      previewSeq.current += 1;
    };
  }, []);

  const loadAll = useCallback(async () => {
    const seq = ++loadSeqRef.current;
    const startDirVersion = getBackupDirVersion();
    const startBackupSaveVersion = getBackupSaveVersion();
    const startReleaseSaveVersion = getReleaseSaveVersion();
    const startBackupSetGen = backupSetGenRef.current;
    const startReleasesGen = ++releasesGenRef.current;
    const startDepsGen = depsGenRef.current;
    activeLoadsRef.current += 1;
    setLoading(true);
    setReleasesLoading(true);

    const trackedReleases = fetchReleases().then(
      (val) => {
        if (mountedRef.current && seq === loadSeqRef.current) {
          setReleasesLoading(false);
          if (
            releasesGenRef.current === startReleasesGen &&
            getReleaseSaveVersion() === startReleaseSaveVersion &&
            !hasActiveReleaseSaves()
          ) {
            setReleases(val);
            setReleasesStale(false);
          } else {
            setReleasesStale(true);
          }
        }
        return val;
      },
      (err) => {
        if (mountedRef.current && seq === loadSeqRef.current) {
          setReleasesLoading(false);
          if (releasesGenRef.current === startReleasesGen) {
            setReleasesStale(true);
          }
        }
        throw err;
      },
    );

    try {
      const [tasksRes, healthRes, qqRes, backupsRes, backupSetRes, releasesRes, depsRes] =
        await Promise.allSettled([
          fetchTasks(),
          fetchHealth(),
          fetchQqLogin(),
          fetchBackups(),
          fetchBackupSettings(),
          trackedReleases,
          fetchDependencies(),
          refreshLiveData(),
        ]);

      if (!mountedRef.current || seq !== loadSeqRef.current) return;

      const warnings: string[] = [];
      if (tasksRes.status === 'fulfilled') {
        setDesired(tasksRes.value.desired);
      } else {
        warnings.push(`运行态目标 (${formatErrorMessage(tasksRes.reason, '读取失败')})`);
      }

      if (healthRes.status === 'fulfilled') {
        setHealth(healthRes.value);
      } else {
        warnings.push(`健康探针 (${formatErrorMessage(healthRes.reason, '读取失败')})`);
      }

      if (qqRes.status === 'fulfilled') {
        setQqLogin(qqRes.value);
      } else {
        warnings.push(`QQ 登录探测 (${formatErrorMessage(qqRes.reason, '读取失败')})`);
      }

      if (backupsRes.status === 'fulfilled') {
        if (getBackupDirVersion() === startDirVersion && !hasPendingBackupDirSaves()) {
          setBackups(backupsRes.value.backups || []);
          setFailedBackups(backupsRes.value.failedAttempts || []);
          setBackupsStale(false);
        } else {
          setBackupsStale(true);
        }
      } else {
        setBackupsStale(true);
        warnings.push(`备份列表 (${formatErrorMessage(backupsRes.reason, '读取失败')})`);
      }

      if (backupSetRes.status === 'fulfilled') {
        if (
          backupSetGenRef.current === startBackupSetGen &&
          getBackupSaveVersion() === startBackupSaveVersion &&
          !hasActiveBackupSaves()
        ) {
          setBackupSettings(backupSetRes.value.settings);
          setBackupSettingsStale(false);
        } else {
          setBackupSettingsStale(true);
        }
      } else {
        setBackupSettingsStale(true);
        warnings.push(`备份设置 (${formatErrorMessage(backupSetRes.reason, '读取失败')})`);
      }

      if (releasesRes.status === 'rejected') {
        warnings.push(`版本更新 (${formatErrorMessage(releasesRes.reason, '读取失败')})`);
      }

      if (depsRes.status === 'fulfilled') {
        if (depsGenRef.current === startDepsGen) {
          setDeps(depsRes.value);
        }
      } else {
        warnings.push(`依赖状态 (${formatErrorMessage(depsRes.reason, '读取失败')})`);
      }

      setLoadNotice(warnings.length > 0 ? `部分运维信息读取失败：${warnings.join('；')}` : null);
    } finally {
      activeLoadsRef.current = Math.max(0, activeLoadsRef.current - 1);
      if (mountedRef.current) setLoading(activeLoadsRef.current > 0);
    }
  }, [refreshLiveData]);

  useEffect(() => {
    void loadAll();
    return subscribeSystemSync(mountIdRef.current, {
      onBackupDirStarted: () => {
        if (!mountedRef.current) return;
        previewSeqRef.current += 1;
        setRestoreTarget(null);
        setBackupsStale(true);
      },
      onBackupSaveSettled: () => {
        if (!mountedRef.current) return;
        setBackupSettingsStale(true);
        if (hasPendingBackupDirSaves()) {
          setBackupsStale(true);
        }
        void loadAll();
      },
      onReleaseSaveSettled: () => {
        if (!mountedRef.current) return;
        setReleasesStale(true);
        void loadAll();
      },
      onActiveSavesChanged: () => {
        if (!mountedRef.current) return;
        setBackupSaveActive(hasActiveBackupSaves());
        setReleaseSaveActive(hasActiveReleaseSaves());
      },
    });
  }, [loadAll]);

  // Reconcile reverted drafts whenever a verified baseline refresh lands or a save attempt finishes.
  useEffect(() => {
    if (!savingBackupRef.current && backupSettings && !backupSettingsStale) {
      setBackupDirty((prev) => pruneRevertedBackupDirty(prev, backupSettings));
    }
  }, [backupSettings, backupSettingsStale, busy]);

  useEffect(() => {
    if (!savingReleaseRef.current && releases?.settings && !releasesStale) {
      const settings = releases.settings;
      setReleaseDirty((prev) => pruneRevertedReleaseDirty(prev, settings));
    }
  }, [releases, releasesStale, busy]);

  return (
    <div className="space-y-4">
      <PageHeader
        title="运维与备份"
        description="管理监督进程生命周期、QQ 登录状态、数据备份与还原预览，以及 HER 与外部依赖版本更新。"
        badge={
          desired ? (
            <Badge
              tone={
                desired.state === 'running'
                  ? 'success'
                  : desired.state === 'maintenance'
                    ? 'warn'
                    : 'danger'
              }
            >
              目标状态：{desired.state}
            </Badge>
          ) : null
        }
        actions={
          <Button size="sm" variant="secondary" disabled={loading} onClick={() => void loadAll()}>
            <RefreshCw className="h-3.5 w-3.5" />
            刷新
          </Button>
        }
      />

      <Tabs
        tabs={[
          { id: 'runtime', label: '运行控制' },
          { id: 'backups', label: '备份与还原' },
          { id: 'updates', label: '更新与依赖' },
        ]}
        active={tab}
        onChange={(id) => setActiveTab('system', id)}
      />

      {loadNotice && (
        <Card className="px-4 py-2.5 text-xs text-[var(--warn)]">{loadNotice}</Card>
      )}
      {unsupportedNotice && (
        <Card className="px-4 py-2.5 text-xs text-[var(--text-secondary)]">
          {unsupportedNotice}
        </Card>
      )}

      {tab === 'runtime' ? (
        <RuntimeControlTab
          busy={busy}
          setBusy={setBusy}
          desired={desired}
          health={health}
          qqLogin={qqLogin}
          qqLoginState={qqLoginState}
          unsupportedNotice={unsupportedNotice}
          setUnsupportedNotice={setUnsupportedNotice}
          mountedRef={mountedRef}
          onRefreshAll={loadAll}
        />
      ) : tab === 'backups' ? (
        <BackupsTab
          mountId={mountIdRef.current}
          busy={busy}
          backupSaveActive={backupSaveActive}
          setBusy={setBusy}
          backups={backups}
          backupsStale={backupsStale}
          setBackupsStale={setBackupsStale}
          failedBackups={failedBackups}
          backupSettings={backupSettings}
          backupSettingsStale={backupSettingsStale}
          setBackupSettings={setBackupSettings}
          setBackupSettingsStale={setBackupSettingsStale}
          backupDirty={backupDirty}
          setBackupDirty={setBackupDirty}
          backupFieldGenRef={backupFieldGenRef}
          savingBackupRef={savingBackupRef}
          backupSetGenRef={backupSetGenRef}
          restoreMemoryOnly={restoreMemoryOnly}
          setRestoreMemoryOnly={setRestoreMemoryOnly}
          restoreTarget={restoreTarget}
          setRestoreTarget={setRestoreTarget}
          previewSeqRef={previewSeqRef}
          mountedRef={mountedRef}
          onRefreshAll={loadAll}
        />
      ) : (
        <UpdatesTab
          mountId={mountIdRef.current}
          busy={busy}
          releaseSaveActive={releaseSaveActive}
          setBusy={setBusy}
          releases={releases}
          releasesStale={releasesStale}
          releasesLoading={releasesLoading}
          setReleases={setReleases}
          setReleasesStale={setReleasesStale}
          releaseDirty={releaseDirty}
          setReleaseDirty={setReleaseDirty}
          releaseFieldGenRef={releaseFieldGenRef}
          savingReleaseRef={savingReleaseRef}
          releasesGenRef={releasesGenRef}
          deps={deps}
          setDeps={setDeps}
          depsGenRef={depsGenRef}
          mountedRef={mountedRef}
          onRefreshAll={loadAll}
        />
      )}
    </div>
  );
};
