import React, { useMemo, useState } from 'react';
import { AlertTriangle, Eye, History } from 'lucide-react';
import { AuditEntry, isRestoreAuditEntry } from '../../api/security';
import { auditEntryKey } from '../../components/AuditInspector';
import { EmptyState, SearchInput, SegmentedControl } from '../../components/ConsoleWidgets';
import { Badge, Button, Card } from '../../components/Primitives';
import { InspectorTarget } from '../../catalog/navigation';
import { formatDateTimeMs } from '../../utils/format';

type AuditFilterKind = 'all' | 'undoable' | 'restore' | 'error';

interface AuditLogTabProps {
  entries: AuditEntry[];
  error: string | null;
  limit: number;
  loading: boolean;
  inspector: InspectorTarget;
  onInspect: (id: string) => void;
  onRetry: () => void;
  onLoadMore: () => void;
}

export const AuditLogTab: React.FC<AuditLogTabProps> = ({
  entries,
  error,
  limit,
  loading,
  inspector,
  onInspect,
  onRetry,
  onLoadMore,
}) => {
  const [search, setSearch] = useState('');
  const [filterKind, setFilterKind] = useState<AuditFilterKind>('all');

  const filteredAudits = useMemo(() => {
    const q = search.trim().toLowerCase();
    return entries.filter((entry) => {
      if (filterKind === 'restore' && !isRestoreAuditEntry(entry)) return false;
      if (
        filterKind === 'undoable' &&
        (isRestoreAuditEntry(entry) ||
          entry.path === '/api/console/token' ||
          entry.files.length === 0)
      ) {
        return false;
      }
      if (filterKind === 'error') {
        const isErr = isRestoreAuditEntry(entry)
          ? entry.result !== 'succeeded' && entry.result !== 'ok' && entry.result !== 'started'
          : entry.status >= 400 || entry.ok === false;
        if (!isErr) return false;
      }
      if (!q) return true;
      if (isRestoreAuditEntry(entry)) {
        return (
          entry.archive.toLowerCase().includes(q) ||
          entry.action.toLowerCase().includes(q) ||
          entry.detail.toLowerCase().includes(q) ||
          entry.scope.toLowerCase().includes(q)
        );
      }
      return (
        entry.id.toLowerCase().includes(q) ||
        entry.path.toLowerCase().includes(q) ||
        (entry.key ?? '').toLowerCase().includes(q) ||
        (entry.error ?? '').toLowerCase().includes(q)
      );
    });
  }, [entries, filterKind, search]);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <div className="w-64">
            <SearchInput
              value={search}
              onChange={setSearch}
              placeholder="搜索路径、ID、对象或报错…"
            />
          </div>
          <SegmentedControl
            value={filterKind}
            onChange={setFilterKind}
            options={[
              { value: 'all', label: '全部' },
              { value: 'undoable', label: '可撤销' },
              { value: 'restore', label: '备份恢复' },
              { value: 'error', label: '异常' },
            ]}
          />
        </div>
        <span className="text-xs text-[var(--text-muted)]">
          当前已载入 {entries.length} 条（命中 {filteredAudits.length} 条）
        </span>
      </div>

      <Card>
        {error ? (
          <div className="p-6">
            <EmptyState
              icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
              title="审计日志读取失败"
              description={error}
              action={
                <Button size="sm" onClick={onRetry}>
                  重新加载
                </Button>
              }
            />
          </div>
        ) : filteredAudits.length === 0 ? (
          <div className="p-6">
            <EmptyState
              icon={<History className="h-6 w-6" />}
              title={loading ? '正在读取审计记录…' : '暂无匹配的审计日志'}
              description="变更白名单、设置项、日程、密码、令牌与恢复操作都会在此留下记录。"
            />
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-xs">
              <thead>
                <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                  <th className="px-4 py-2 font-medium">时间</th>
                  <th className="px-3 py-2 font-medium">动作 / 路径</th>
                  <th className="px-3 py-2 font-medium">结果</th>
                  <th className="px-3 py-2 font-medium">文件摘要 / 说明</th>
                  <th className="px-4 py-2 text-right font-medium">检视</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--border-hairline)]">
                {filteredAudits.map((entry) => {
                  const entryId = auditEntryKey(entry);
                  const selected = inspector?.kind === 'audit' && inspector.id === entryId;
                  if (isRestoreAuditEntry(entry)) {
                    const restoreOk = entry.result === 'succeeded' || entry.result === 'ok';
                    const restoreStarted = entry.result === 'started';
                    return (
                      <tr
                        key={entryId}
                        onClick={() => onInspect(entryId)}
                        className={`cursor-pointer transition-colors hover:bg-[var(--bg-hover)] ${
                          selected ? 'bg-[var(--accent-soft)]/50' : ''
                        }`}
                      >
                        <td className="whitespace-nowrap px-4 py-2 text-[11px] text-[var(--text-muted)]">
                          {formatDateTimeMs(entry.at)}
                        </td>
                        <td className="px-3 py-2">
                          <div className="flex items-center gap-1.5">
                            <Badge tone="warn">backup.restore</Badge>
                            <span className="font-mono text-[11px]">{entry.archive}</span>
                          </div>
                        </td>
                        <td className="px-3 py-2">
                          <Badge tone={restoreOk ? 'success' : restoreStarted ? 'accent' : 'danger'}>
                            {entry.result}
                          </Badge>
                        </td>
                        <td className="px-3 py-2 text-[11px] text-[var(--text-secondary)]">
                          scope={entry.scope} · by={entry.by}
                        </td>
                        <td className="px-4 py-2 text-right">
                          <Button
                            size="sm"
                            variant="ghost"
                            onClick={(e) => {
                              e.stopPropagation();
                              onInspect(entryId);
                            }}
                          >
                            <Eye className="h-3.5 w-3.5" />
                            详情
                          </Button>
                        </td>
                      </tr>
                    );
                  }

                  const tokenRotate = entry.path === '/api/console/token';
                  const okStatus = entry.status < 400 && entry.ok !== false;
                  return (
                    <tr
                      key={entryId}
                      onClick={() => onInspect(entryId)}
                      className={`cursor-pointer transition-colors hover:bg-[var(--bg-hover)] ${
                        selected ? 'bg-[var(--accent-soft)]/50' : ''
                      }`}
                    >
                      <td className="whitespace-nowrap px-4 py-2 text-[11px] text-[var(--text-muted)]">
                        {formatDateTimeMs(entry.ts)}
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex items-center gap-1.5">
                          <Badge tone="neutral">{entry.method}</Badge>
                          <span className="font-mono text-[11px] font-medium text-[var(--text-primary)]">
                            {entry.path}
                          </span>
                        </div>
                      </td>
                      <td className="px-3 py-2">
                        <Badge tone={okStatus ? 'success' : 'danger'}>HTTP {entry.status}</Badge>
                      </td>
                      <td className="px-3 py-2 text-[11px] text-[var(--text-secondary)]">
                        {entry.error ? (
                          <span className="text-[var(--danger)]">{entry.error}</span>
                        ) : (
                          <span>
                            {entry.files.length} 个文件快照
                            {entry.key ? ` · ${entry.key}` : ''}
                            {tokenRotate ? '（不可撤销）' : ''}
                          </span>
                        )}
                      </td>
                      <td className="px-4 py-2 text-right">
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={(e) => {
                            e.stopPropagation();
                            onInspect(entryId);
                          }}
                        >
                          <Eye className="h-3.5 w-3.5" />
                          {entry.files.length > 0 && !tokenRotate ? '检视 / 撤销' : '详情'}
                        </Button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      {entries.length >= limit && limit < 200 && (
        <div className="flex justify-center">
          <Button size="sm" onClick={onLoadMore} disabled={loading}>
            加载更多历史记录（当前上限 {limit}）
          </Button>
        </div>
      )}
    </div>
  );
};
