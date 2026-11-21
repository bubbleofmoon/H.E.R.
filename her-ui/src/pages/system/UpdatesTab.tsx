import React from 'react';
import { ArrowUpCircle, PackageCheck } from 'lucide-react';
import { ApiRequestError } from '../../api/client';
import {
  DependenciesResponse,
  ReleasesStatusResponse,
  approveDependency,
  approveRelease,
  beginReleaseSave,
  checkDependencies,
  fetchReleases,
  finishReleaseSave,
  hasActiveReleaseSaves,
  saveReleaseSettings,
} from '../../api/system';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatErrorMessage, formatRuntimeTime } from '../../utils/systemFormat';

export interface ReleaseDirtyPatch {
  policy?: 'off' | 'notify' | 'auto';
  feedUrl?: string;
}

export function pruneRevertedReleaseDirty(
  prev: ReleaseDirtyPatch,
  settings: ReleasesStatusResponse['settings'],
): ReleaseDirtyPatch {
  let changed = false;
  const next: ReleaseDirtyPatch = { ...prev };
  if (next.policy !== undefined && next.policy === settings.policy) {
    delete next.policy;
    changed = true;
  }
  if (
    next.feedUrl !== undefined &&
    next.feedUrl.trim() === (settings.feedUrl || '').trim()
  ) {
    delete next.feedUrl;
    changed = true;
  }
  return changed ? next : prev;
}

interface UpdatesTabProps {
  mountId: number;
  busy: boolean;
  releaseSaveActive: boolean;
  setBusy: React.Dispatch<React.SetStateAction<boolean>>;
  releases: ReleasesStatusResponse | null;
  releasesStale: boolean;
  releasesLoading: boolean;
  setReleases: React.Dispatch<React.SetStateAction<ReleasesStatusResponse | null>>;
  setReleasesStale: React.Dispatch<React.SetStateAction<boolean>>;
  releaseDirty: ReleaseDirtyPatch;
  setReleaseDirty: React.Dispatch<React.SetStateAction<ReleaseDirtyPatch>>;
  releaseFieldGenRef: React.MutableRefObject<Partial<Record<keyof ReleaseDirtyPatch, number>>>;
  savingReleaseRef: React.MutableRefObject<boolean>;
  releasesGenRef: React.MutableRefObject<number>;
  deps: DependenciesResponse | null;
  setDeps: React.Dispatch<React.SetStateAction<DependenciesResponse | null>>;
  depsGenRef: React.MutableRefObject<number>;
  mountedRef: React.MutableRefObject<boolean>;
  onRefreshAll: () => Promise<void>;
}

export const UpdatesTab: React.FC<UpdatesTabProps> = ({
  mountId,
  busy,
  releaseSaveActive,
  setBusy,
  releases,
  releasesStale,
  releasesLoading,
  setReleases,
  setReleasesStale,
  releaseDirty,
  setReleaseDirty,
  releaseFieldGenRef,
  savingReleaseRef,
  releasesGenRef,
  deps,
  setDeps,
  depsGenRef,
  mountedRef,
  onRefreshAll,
}) => {
  const { openConfirm, pushToast, refreshLiveData } = useConsole();

  const releasesReady = releases !== null && !releasesStale && !releasesLoading;
  const effectivePolicy =
    releaseDirty.policy !== undefined
      ? releaseDirty.policy
      : releases?.settings?.policy || 'notify';
  const effectiveFeedUrl =
    releaseDirty.feedUrl !== undefined
      ? releaseDirty.feedUrl
      : releases?.settings?.feedUrl || '';
  const serverPolicyOff = !releasesReady || releases.settings?.policy === 'off';
  const releaseApprovalPending = Boolean(releases?.approval);
  const depApprovalPending = Boolean(deps?.approval) || deps?.result?.status === 'running';
  const hasReleaseDirty = Object.keys(releaseDirty).length > 0;

  const updateReleaseField = (field: keyof ReleaseDirtyPatch, patch: ReleaseDirtyPatch) => {
    if (!releases) return;
    releaseFieldGenRef.current[field] = (releaseFieldGenRef.current[field] ?? 0) + 1;
    setReleaseDirty((prev) => {
      const next: ReleaseDirtyPatch = { ...prev, ...patch };
      return savingReleaseRef.current || releasesStale
        ? next
        : pruneRevertedReleaseDirty(next, releases.settings);
    });
  };

  const handleSaveReleaseSettings = async () => {
    if (busy || hasActiveReleaseSaves() || releases === null || !hasReleaseDirty) return;
    const payload: { policy?: 'off' | 'notify' | 'auto'; feedUrl?: string } = {};
    const submittedGens: Partial<Record<keyof ReleaseDirtyPatch, number>> = {};

    if (releaseDirty.policy !== undefined) {
      payload.policy = releaseDirty.policy;
      submittedGens.policy = releaseFieldGenRef.current.policy ?? 0;
    }
    if (releaseDirty.feedUrl !== undefined) {
      const feedUrl = releaseDirty.feedUrl.trim();
      if (!feedUrl) {
        pushToast('更新源 Feed URL 不能为空', 'warn');
        return;
      }
      payload.feedUrl = feedUrl;
      submittedGens.feedUrl = releaseFieldGenRef.current.feedUrl ?? 0;
    }

    savingReleaseRef.current = true;
    beginReleaseSave();
    setBusy(true);
    let finalized = false;
    const finalizeSave = () => {
      if (!finalized) {
        finalized = true;
        finishReleaseSave(mountId);
      }
    };

    try {
      const res = await saveReleaseSettings(payload);
      releasesGenRef.current += 1;
      savingReleaseRef.current = false;
      finalizeSave();
      if (mountedRef.current) {
        setReleases((prev) => (prev ? { ...prev, settings: res.settings } : prev));
        setReleasesStale(false);
        setReleaseDirty((prev) => {
          const next = { ...prev };
          for (const [k, sGen] of Object.entries(submittedGens) as [
            keyof ReleaseDirtyPatch,
            number,
          ][]) {
            if ((releaseFieldGenRef.current[k] ?? 0) === sGen) {
              delete next[k];
            }
          }
          return pruneRevertedReleaseDirty(next, res.settings);
        });
      }
      pushToast('已保存更新策略', 'success');
      if (mountedRef.current) await onRefreshAll();
    } catch (err) {
      savingReleaseRef.current = false;
      finalizeSave();
      pushToast(formatErrorMessage(err, '保存更新策略失败'), 'danger');
      const isDefinitiveClientReject =
        err instanceof ApiRequestError && err.status >= 400 && err.status < 500;
      if (!isDefinitiveClientReject && mountedRef.current) {
        setReleasesStale(true);
        await onRefreshAll();
      }
    } finally {
      savingReleaseRef.current = false;
      finalizeSave();
      if (mountedRef.current) setBusy(false);
    }
  };

  const verifyLiveReleasePolicy = async (forDepAction = false): Promise<boolean> => {
    const readGen = ++releasesGenRef.current;
    let latestReleases: ReleasesStatusResponse;
    try {
      latestReleases = await fetchReleases();
    } catch (err) {
      if (mountedRef.current && releasesGenRef.current === readGen) {
        setReleasesStale(true);
      }
      throw err;
    }
    if (!mountedRef.current) return false;
    if (releasesGenRef.current !== readGen) {
      pushToast('更新策略状态已刷新，请确认当前状态后再试', 'info');
      return false;
    }
    setReleases(latestReleases);
    setReleasesStale(false);
    if (latestReleases.settings?.policy === 'off') {
      pushToast(
        forDepAction
          ? '当前更新策略已为 off，已取消依赖操作'
          : '当前更新策略已关闭，未提交审批',
        'warn',
      );
      return false;
    }
    if (!forDepAction && latestReleases.approval) {
      pushToast('当前已有待切换审批，未提交', 'warn');
      return false;
    }
    return true;
  };

  const handleApproveRelease = (tag: string) => {
    if (releaseApprovalPending || serverPolicyOff) return;
    openConfirm({
      title: `批准升级 HER 到版本 ${tag}？`,
      description: '确认后将安排版本替换并在完成后重启服务；需在当前安装型运行时下可执行。',
      confirmLabel: '批准升级',
      danger: false,
      onConfirm: async () => {
        setBusy(true);
        try {
          if (!(await verifyLiveReleasePolicy(false))) return;
          const res = await approveRelease(tag);
          releasesGenRef.current += 1;
          pushToast(`已批准升级至 ${res.approved}`, 'success');
          await Promise.all([
            mountedRef.current ? onRefreshAll() : Promise.resolve(),
            refreshLiveData(),
          ]);
        } catch (err) {
          pushToast(formatErrorMessage(err, '批准升级失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleCheckDeps = async () => {
    if (busy || serverPolicyOff) return;
    setBusy(true);
    try {
      if (!(await verifyLiveReleasePolicy(true))) return;
      const res = await checkDependencies();
      depsGenRef.current += 1;
      if (mountedRef.current) setDeps(res);
      pushToast('已完成外部依赖版本检查', 'success');
    } catch (err) {
      pushToast(formatErrorMessage(err, '检查依赖更新失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  const handleApproveDep = (name: string, version: string) => {
    if (depApprovalPending || serverPolicyOff) return;
    openConfirm({
      title: `批准升级依赖 ${name} -> ${version}？`,
      description: `将更新锁定版本（Pinned version）并按监督进程流程切换 ${name}。`,
      confirmLabel: '批准升级',
      danger: false,
      onConfirm: async () => {
        setBusy(true);
        try {
          if (!(await verifyLiveReleasePolicy(true))) return;
          const res = await approveDependency(name, version);
          depsGenRef.current += 1;
          pushToast(`已批准 ${res.name} 升级至 ${res.approved}`, 'success');
          if (mountedRef.current) await onRefreshAll();
        } catch (err) {
          pushToast(formatErrorMessage(err, '批准依赖升级失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-1.5">
              <ArrowUpCircle className="h-4 w-4 text-[var(--accent)]" />
              HER 版本与更新策略
            </span>
          }
          subtitle="检查发布版本、设置自动策略及批准应用更新"
        />
        <div className="space-y-4 p-4 text-xs">
          <dl className="grid grid-cols-2 gap-2 rounded bg-[var(--bg-subtle)]/60 p-3">
            <div>
              <dt className="text-[var(--text-muted)]">当前版本</dt>
              <dd className="font-mono font-medium">{releases?.current || '未知'}</dd>
            </div>
            <div>
              <dt className="text-[var(--text-muted)]">可用新版本</dt>
              <dd className="font-mono font-medium text-[var(--accent)]">
                {releases?.available?.tag || '暂无更新'}
              </dd>
            </div>
            <div>
              <dt className="text-[var(--text-muted)]">上次检查</dt>
              <dd className="tabular-nums">{formatRuntimeTime(releases?.lastCheckedAt)}</dd>
            </div>
            <div>
              <dt className="text-[var(--text-muted)]">已批准待切换</dt>
              <dd className="font-mono">{releases?.approval?.release?.tag || '无'}</dd>
            </div>
          </dl>

          {releases?.error && (
            <div className="text-[var(--warn)]">更新检查告警：{releases.error}</div>
          )}
          {releases?.updateError && (
            <div className="text-[var(--danger)]">更新应用异常：{releases.updateError}</div>
          )}

          {releases?.available && (
            <div className="flex items-center justify-between rounded border border-blue-200 bg-[var(--accent-soft)] p-3 dark:border-blue-900">
              <span>
                检测到新版本 {releases.available.tag}
                {releaseApprovalPending ? '（已有待切换更新）' : ''}
              </span>
              <Button
                size="sm"
                variant="primary"
                disabled={busy || releaseApprovalPending || serverPolicyOff}
                onClick={() => handleApproveRelease(releases.available!.tag)}
              >
                {releaseApprovalPending ? '已批准等待切换' : '批准更新'}
              </Button>
            </div>
          )}

          <div className="space-y-2 border-t border-[var(--border-hairline)] pt-3">
            <label className="block space-y-1">
              <span className="text-[var(--text-secondary)]">更新策略</span>
              <select
                disabled={releases === null}
                value={effectivePolicy}
                onChange={(e) => {
                  const v = e.target.value;
                  updateReleaseField('policy', {
                    policy: v === 'off' || v === 'auto' ? v : 'notify',
                  });
                }}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs disabled:opacity-50"
              >
                <option value="off">关闭检查 (off)</option>
                <option value="notify">仅通知 (notify)</option>
                <option value="auto">自动下载并等待切换 (auto)</option>
              </select>
            </label>
            <label className="block space-y-1">
              <span className="text-[var(--text-secondary)]">更新源 Feed URL</span>
              <input
                type="text"
                disabled={releases === null}
                value={effectiveFeedUrl}
                onChange={(e) => updateReleaseField('feedUrl', { feedUrl: e.target.value })}
                className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs disabled:opacity-50"
              />
            </label>
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || releaseSaveActive || releases === null || !hasReleaseDirty}
              onClick={() => void handleSaveReleaseSettings()}
            >
              保存更新策略
            </Button>
          </div>
        </div>
      </Card>

      <Card>
        <CardHeader
          title={
            <span className="inline-flex items-center gap-1.5">
              <PackageCheck className="h-4 w-4 text-[var(--accent)]" />
              外部依赖版本管理
            </span>
          }
          subtitle="查看与升级 snowluma / dsh / relay 锁定版本"
          actions={
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || serverPolicyOff}
              onClick={() => void handleCheckDeps()}
            >
              检查依赖更新
            </Button>
          }
        />
        <div className="space-y-3 p-4 text-xs">
          {!releasesReady ? (
            <div className="rounded bg-[var(--bg-subtle)] px-3 py-2 text-[var(--text-muted)]">
              最新更新策略状态尚未确认或正在读取中，远端依赖检查与升级操作已锁定。
            </div>
          ) : serverPolicyOff ? (
            <div className="rounded bg-[var(--bg-subtle)] px-3 py-2 text-[var(--text-muted)]">
              当前更新策略为 <code className="font-mono">off</code>，远端依赖检查与升级批准均已停用。
            </div>
          ) : null}
          {deps?.updateError && (
            <div className="text-[var(--danger)]">依赖升级异常：{deps.updateError}</div>
          )}
          {deps?.approval && (
            <div className="rounded bg-[var(--accent-soft)] px-3 py-2 text-[var(--accent)]">
              待执行升级：{deps.approval.name} ({deps.approval.current} → {deps.approval.version})
            </div>
          )}
          <div className="divide-y divide-[var(--border-hairline)] rounded border border-[var(--border-hairline)]">
            {!deps?.dependencies ? (
              <div className="p-4 text-center text-[var(--text-muted)]">暂无依赖信息</div>
            ) : (
              Object.entries(deps.dependencies).map(([name, view]) => (
                <div key={name} className="flex items-center justify-between gap-2 p-3">
                  <div className="min-w-0 space-y-0.5">
                    <div className="flex items-center gap-2">
                      <span className="font-mono font-semibold">{name}</span>
                      <Badge tone="neutral">当前 {view.current || '未知'}</Badge>
                      {view.available && (
                        <Badge tone="accent">可升级 {view.available}</Badge>
                      )}
                    </div>
                    <div className="text-[11px] text-[var(--text-muted)]">
                      最新：{view.latest || '—'} · 来源：{view.source || view.sourceUnavailable || '—'}
                    </div>
                    {view.error && (
                      <div className="text-[11px] text-[var(--danger)]">{view.error}</div>
                    )}
                  </div>
                  {view.available && (
                    <Button
                      size="sm"
                      variant="primary"
                      disabled={busy || depApprovalPending || serverPolicyOff}
                      onClick={() => handleApproveDep(name, view.available!)}
                    >
                      {depApprovalPending ? '升级执行/待切中' : '批准升级'}
                    </Button>
                  )}
                </div>
              ))
            )}
          </div>
          {deps?.result && (
            <div className="rounded bg-[var(--bg-subtle)]/60 p-2.5 text-[11px] text-[var(--text-secondary)]">
              最近依赖变更：{deps.result.name} {deps.result.from} → {deps.result.to}（
              {deps.result.status}）{deps.result.reason ? `：${deps.result.reason}` : ''}
            </div>
          )}
        </div>
      </Card>
    </div>
  );
};
