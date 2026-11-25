import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  AlertTriangle,
  ArrowUpRight,
  FileCheck2,
  History,
  RefreshCw,
  RotateCcw,
} from 'lucide-react';
import { ApiRequestError } from '../api/client';
import {
  AuditEntry,
  StandardAuditEntry,
  fetchAuditRecent,
  isRestoreAuditEntry,
  undoAuditEntry,
} from '../api/security';
import { EmptyState, KeyValueRow } from './ConsoleWidgets';
import { Badge, Button, Card } from './Primitives';
import { useConsole } from '../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage, shortDigest } from '../utils/format';

export const AUDIT_UPDATED_EVENT = 'her:audit-updated';

export function auditEntryKey(entry: AuditEntry): string {
  if (isRestoreAuditEntry(entry)) {
    return `restore:${entry.at}`;
  }
  return entry.id;
}

export const AuditInspector: React.FC<{ auditId: string }> = ({ auditId }) => {
  const { openConfirm, pushToast, refreshLiveData, navigate } = useConsole();
  const [entry, setEntry] = useState<AuditEntry | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [undoing, setUndoing] = useState(false);
  const [undoResult, setUndoResult] = useState<{
    id: string;
    restored: string[];
    revoked?: string[];
    settingsKept?: string[];
  } | null>(null);
  const mountedRef = useRef(true);
  const loadGenRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadEntry = useCallback(async () => {
    const gen = ++loadGenRef.current;
    setLoading(true);
    setError(null);
    try {
      const res = await fetchAuditRecent(200);
      if (!mountedRef.current || gen !== loadGenRef.current) return;
      const found = res.entries.find((item) => auditEntryKey(item) === auditId) ?? null;
      setEntry(found);
      if (!found) {
        setError('在最近 200 条审计日志中未找到该记录，可能已过期。');
      }
    } catch (err) {
      if (!mountedRef.current || gen !== loadGenRef.current) return;
      setError(formatErrorMessage(err, '加载审计详情失败'));
    } finally {
      if (mountedRef.current && gen === loadGenRef.current) {
        setLoading(false);
      }
    }
  }, [auditId]);

  useEffect(() => {
    void loadEntry();
  }, [loadEntry]);

  const executeUndo = async (stdEntry: StandardAuditEntry, forceConfirm: boolean) => {
    setUndoing(true);
    try {
      const res = await undoAuditEntry(stdEntry.id, forceConfirm);
      if (!mountedRef.current) return;
      setUndoResult({
        id: res.id,
        restored: res.restored ?? [],
        revoked: res.revoked,
        settingsKept: res.settingsKept,
      });
      pushToast(`已撤销操作 ${stdEntry.path}`, 'success');
      window.dispatchEvent(new CustomEvent(AUDIT_UPDATED_EVENT));
      await Promise.all([loadEntry(), refreshLiveData()]);
    } catch (err) {
      if (!mountedRef.current) return;
      const needsConfirm =
        err instanceof ApiRequestError &&
        err.status === 409 &&
        !forceConfirm &&
        (err.message.includes('确认') || err.message.toLowerCase().includes('confirm'));
      if (needsConfirm) {
        window.setTimeout(() => {
          if (!mountedRef.current) return;
          openConfirm({
            title: '需要二次安全确认',
            description: `后端要求对安全类变更进行二次确认 (${err.message})。撤销该变更会影响控制台访问策略，确认强制执行撤销吗？`,
            confirmLabel: '强制确认撤销',
            danger: true,
            onConfirm: () => executeUndo(stdEntry, true),
          });
        }, 0);
        return;
      }
      pushToast(formatErrorMessage(err, '撤销失败'), 'danger');
    } finally {
      if (mountedRef.current) {
        setUndoing(false);
      }
    }
  };

  const requestUndo = (stdEntry: StandardAuditEntry) => {
    openConfirm({
      title: '确认撤销该审计变更？',
      description: `将按变更前快照回滚 ${stdEntry.method} ${stdEntry.path}（记录 ID: ${stdEntry.id}）。若涉及安全策略或文件已被后续修改，后端可能拦截。`,
      confirmLabel: '确认撤销',
      danger: true,
      onConfirm: () => executeUndo(stdEntry, false),
    });
  };

  if (loading && !entry) {
    return (
      <div className="flex items-center gap-2 py-8 text-xs text-[var(--text-muted)]">
        <RefreshCw className="h-3.5 w-3.5 animate-spin" />
        <span>正在读取审计记录详情…</span>
      </div>
    );
  }

  if (!entry) {
    return (
      <EmptyState
        icon={<History className="h-6 w-6" />}
        title="未找到审计记录"
        description={error || '未匹配到对应的审计条目。'}
        action={
          <Button size="sm" onClick={() => void loadEntry()}>
            重试加载
          </Button>
        }
      />
    );
  }

  if (isRestoreAuditEntry(entry)) {
    const restoreOk = entry.result === 'succeeded' || entry.result === 'ok';
    const restoreStarted = entry.result === 'started';
    const restoreTone = restoreOk ? 'success' : restoreStarted ? 'accent' : 'danger';
    return (
      <div className="space-y-4">
        <div className="flex items-center justify-between gap-2">
          <Badge tone={restoreOk ? 'success' : restoreStarted ? 'accent' : 'warn'}>
            备份恢复记录
          </Badge>
          <span className="font-mono text-[11px] text-[var(--text-muted)]">
            {formatDateTimeMs(entry.at)}
          </span>
        </div>

        <Card className="p-3">
          <KeyValueRow label="动作" value={<span className="font-mono">{entry.action}</span>} />
          <KeyValueRow label="归档文件" value={<span className="font-mono">{entry.archive}</span>} />
          <KeyValueRow label="恢复范围" value={entry.scope} />
          <KeyValueRow label="执行来源" value={entry.by} />
          <KeyValueRow
            label="结果"
            value={
              <Badge tone={restoreTone}>{entry.result}</Badge>
            }
          />
          {entry.rollbackStatus && <KeyValueRow label="回滚状态" value={entry.rollbackStatus} />}
          {entry.preRestoreArchive && (
            <KeyValueRow
              label="恢复前备份"
              value={<span className="font-mono">{entry.preRestoreArchive}</span>}
            />
          )}
        </Card>

        {entry.detail && (
          <Card className="p-3">
            <div className="mb-1 text-[11px] font-semibold text-[var(--text-muted)]">详细信息</div>
            <p className="whitespace-pre-wrap break-all font-mono text-[11px] text-[var(--text-secondary)]">
              {entry.detail}
            </p>
          </Card>
        )}

        <div className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-2.5 text-[11px] text-[var(--text-muted)]">
          备份恢复审计记录仅作归档追踪，如需再次恢复请前往「系统维护 / 备份与恢复」选择还原前备份档。
        </div>
      </div>
    );
  }

  const tokenRotation = entry.path === '/api/console/token';
  const hasUndoFiles = Array.isArray(entry.files) && entry.files.length > 0;
  const settingsChange =
    entry.path.startsWith('/api/settings') ||
    entry.path === '/api/whitelist' ||
    entry.path === '/api/security';

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-1.5">
          <Badge tone="neutral">{entry.method}</Badge>
          <Badge tone={entry.status >= 400 || entry.ok === false ? 'danger' : 'success'}>
            HTTP {entry.status}
          </Badge>
        </div>
        <span className="font-mono text-[11px] text-[var(--text-muted)]">
          {formatDateTimeMs(entry.ts)}
        </span>
      </div>

      <Card className="p-3">
        <KeyValueRow label="审计 ID" value={<span className="font-mono">{entry.id}</span>} />
        <KeyValueRow label="接口路径" value={<span className="font-mono">{entry.path}</span>} />
        {entry.key && (
          <KeyValueRow label="关联对象" value={<span className="font-mono">{entry.key}</span>} />
        )}
        {entry.error && (
          <KeyValueRow
            label="错误信息"
            value={<span className="text-[var(--danger)]">{entry.error}</span>}
          />
        )}
      </Card>

      {settingsChange && (
        <div className="flex items-center justify-between gap-2 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] px-3 py-2 text-xs">
          <span className="text-[var(--text-secondary)]">
            审计日志记录文件摘要；设置字段值前/后对比请查看设置变更历史。
          </span>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => navigate('settings', 'history')}
          >
            查看变更历史
            <ArrowUpRight className="h-3.5 w-3.5" />
          </Button>
        </div>
      )}

      <div className="space-y-2">
        <div className="flex items-center gap-1.5 text-xs font-semibold text-[var(--text-primary)]">
          <FileCheck2 className="h-3.5 w-3.5 text-[var(--accent)]" />
          <span>文件校验摘要 ({entry.files.length})</span>
        </div>
        {entry.files.length === 0 ? (
          <div className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3 text-xs text-[var(--text-muted)]">
            该条操作未关联磁盘文件快照差异。
          </div>
        ) : (
          <div className="space-y-2">
            {entry.files.map((fileDiff) => (
              <Card key={`${fileDiff.file}:${fileDiff.backup}`} className="space-y-1 p-2.5">
                <div className="truncate font-mono text-[11px] font-medium text-[var(--text-primary)]">
                  {fileDiff.file}
                </div>
                <div className="grid grid-cols-[68px_1fr] gap-x-2 gap-y-0.5 font-mono text-[10.5px] text-[var(--text-muted)]">
                  <span>beforeSha</span>
                  <span className="truncate" title={fileDiff.beforeSha}>
                    {shortDigest(fileDiff.beforeSha)}
                  </span>
                  <span>afterSha</span>
                  <span className="truncate" title={fileDiff.afterSha}>
                    {shortDigest(fileDiff.afterSha)}
                  </span>
                  {fileDiff.afterRegistryFreeSha && (
                    <>
                      <span>regFreeSha</span>
                      <span className="truncate" title={fileDiff.afterRegistryFreeSha}>
                        {shortDigest(fileDiff.afterRegistryFreeSha)}
                      </span>
                    </>
                  )}
                  <span>backup</span>
                  <span className="truncate" title={fileDiff.backup}>
                    {fileDiff.backup || '—'}
                  </span>
                </div>
              </Card>
            ))}
          </div>
        )}
      </div>

      {undoResult && (
        <Card className="border-emerald-300 bg-[var(--success-soft)]/30 p-3 text-xs dark:border-emerald-900">
          <div className="font-semibold text-[var(--success)]">撤销已执行</div>
          <div className="mt-1 space-y-0.5 text-[11px] text-[var(--text-secondary)]">
            <div>已恢复文件：{undoResult.restored.join(', ') || '无'}</div>
            {undoResult.revoked && undoResult.revoked.length > 0 && (
              <div>已吊销会话/令牌：{undoResult.revoked.join(', ')}</div>
            )}
            {undoResult.settingsKept && undoResult.settingsKept.length > 0 && (
              <div>保留未回滚设置：{undoResult.settingsKept.join(', ')}</div>
            )}
          </div>
        </Card>
      )}

      <div className="border-t border-[var(--border-hairline)] pt-3">
        {tokenRotation ? (
          <div className="flex items-start gap-2 rounded-md border border-amber-300 bg-[var(--warn-soft)] p-2.5 text-xs text-[var(--warn)] dark:border-amber-900">
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>控制台令牌轮换属于不可逆凭据吊销，不能通过审计撤销恢复旧令牌。</span>
          </div>
        ) : (
          <div className="flex items-center justify-between gap-2">
            <span className="text-[11px] text-[var(--text-muted)]">
              {hasUndoFiles ? '基于修改前备份文件恢复内容并校验哈希' : '无文件快照时撤销可能被拒绝'}
            </span>
            <Button
              size="sm"
              variant="danger"
              disabled={undoing || !hasUndoFiles}
              onClick={() => requestUndo(entry)}
            >
              <RotateCcw className="h-3.5 w-3.5" />
              {undoing ? '撤销中…' : '撤销该变更'}
            </Button>
          </div>
        )}
      </div>
    </div>
  );
};
