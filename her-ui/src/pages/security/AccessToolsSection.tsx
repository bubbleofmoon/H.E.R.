import React, { useRef, useState } from 'react';
import { AlertTriangle, Copy, KeyRound, UserCheck } from 'lucide-react';
import {
  MemoryAclRow,
  authorizeReadCheck,
  grantMemoryAcl,
  revokeMemoryAcl,
  rotateConsoleToken,
} from '../../api/security';
import { EmptyState, KeyValueRow } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatErrorMessage } from '../../utils/format';

const INPUT_CLASS =
  'w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-1.5 text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50';

interface AccessToolsSectionProps {
  aclRows: MemoryAclRow[];
  aclError: string | null;
  setAclRows: React.Dispatch<React.SetStateAction<MemoryAclRow[]>>;
  setAclError: React.Dispatch<React.SetStateAction<string | null>>;
  aclGenRef: React.MutableRefObject<number>;
  mountedRef: React.MutableRefObject<boolean>;
  onRetryLoad: () => void;
}

export const AccessToolsSection: React.FC<AccessToolsSectionProps> = ({
  aclRows,
  aclError,
  setAclRows,
  setAclError,
  aclGenRef,
  mountedRef,
  onRetryLoad,
}) => {
  const { openConfirm, pushToast, refreshAuthAndShell } = useConsole();

  const [readKey, setReadKey] = useState('');
  const [readToken, setReadToken] = useState('');
  const [readResult, setReadResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [readBusy, setReadBusy] = useState(false);

  const [aclForm, setAclForm] = useState({
    readerType: 'session' as 'session' | 'person' | 'team',
    readerId: '',
    readType: 'session' as 'session' | 'person' | 'team' | '*',
    readId: '',
  });
  const [aclBusy, setAclBusy] = useState(false);
  const aclBusyRef = useRef(false);

  const [customToken, setCustomToken] = useState('');
  const [rotatedToken, setRotatedToken] = useState<string | null>(null);
  const [rotateBusy, setRotateBusy] = useState(false);

  const wildcardScope =
    aclForm.readType === '*' ||
    (aclForm.readType === 'session' && aclForm.readId.trim() === '*');

  const handleReadCheck = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!readKey.trim() || !readToken.trim()) {
      pushToast('请填写会话 key 与待验证令牌', 'warn');
      return;
    }
    setReadBusy(true);
    try {
      const res = await authorizeReadCheck(readKey.trim(), readToken);
      if (!mountedRef.current) return;
      setReadResult({ ok: Boolean(res.ok), text: `授权通过：${res.key}` });
      setReadToken('');
    } catch (err) {
      if (!mountedRef.current) return;
      setReadResult({ ok: false, text: formatErrorMessage(err, '授权校验未通过') });
    } finally {
      if (mountedRef.current) setReadBusy(false);
    }
  };

  const handleGrantAcl = async (e: React.FormEvent) => {
    e.preventDefault();
    if (aclBusyRef.current || aclBusy) return;
    if (!aclForm.readerId.trim()) {
      pushToast('请填写读取方 ID (readerId)', 'warn');
      return;
    }
    const effectiveReadId = aclForm.readType === '*' ? '*' : aclForm.readId.trim();
    if (aclForm.readType !== '*' && !effectiveReadId) {
      pushToast('请填写目标 readId，或切换目标类型为通配 *', 'warn');
      return;
    }
    aclBusyRef.current = true;
    setAclBusy(true);
    try {
      const res = await grantMemoryAcl({
        readerType: aclForm.readerType,
        readerId: aclForm.readerId.trim(),
        readType: aclForm.readType,
        readId: effectiveReadId,
        level: 'read',
      });
      aclGenRef.current += 1;
      if (!mountedRef.current) return;
      setAclRows(res.acl);
      setAclError(null);
      setAclForm((prev) => ({ ...prev, readerId: '', readId: '' }));
      pushToast(`已授予记忆读取权限 (#${res.id})`, 'success');
    } catch (err) {
      pushToast(formatErrorMessage(err, '授予 ACL 失败'), 'danger');
    } finally {
      aclBusyRef.current = false;
      if (mountedRef.current) setAclBusy(false);
    }
  };

  const requestRevokeAcl = (row: MemoryAclRow) => {
    if (aclBusyRef.current || aclBusy) return;
    openConfirm({
      title: '确认撤销该条记忆访问权限？',
      description: `读取方 ${row.reader_type}:${row.reader_id} 将失去对 ${row.read_type}:${row.read_id} (${row.level}) 的读取授权。`,
      confirmLabel: '确认撤销',
      danger: true,
      onConfirm: async () => {
        if (aclBusyRef.current) return;
        aclBusyRef.current = true;
        setAclBusy(true);
        try {
          const res = await revokeMemoryAcl({
            readerType: row.reader_type,
            readerId: row.reader_id,
            readType: row.read_type,
            readId: row.read_id,
          });
          aclGenRef.current += 1;
          if (!mountedRef.current) return;
          setAclRows(res.acl);
          setAclError(null);
          pushToast(
            res.removed ? '已撤销该条记忆访问权限' : '未匹配到可撤销的授权条目',
            res.removed ? 'success' : 'info',
          );
        } catch (err) {
          pushToast(formatErrorMessage(err, '撤销 ACL 失败'), 'danger');
        } finally {
          aclBusyRef.current = false;
          if (mountedRef.current) setAclBusy(false);
        }
      },
    });
  };

  const requestRotateToken = () => {
    openConfirm({
      title: '危险操作：轮换控制台令牌？',
      description:
        '轮换后旧令牌立即失效，所有浏览器会话将被签出且当前浏览器会话凭证会被清除，此操作不可通过审计撤销。新令牌将在本页仅展示一次，请务必复制保存后再点击重新登录。',
      confirmLabel: '确认轮换令牌',
      danger: true,
      onConfirm: async () => {
        setRotateBusy(true);
        try {
          const res = await rotateConsoleToken(customToken);
          if (!mountedRef.current) return;
          setRotatedToken(res.token);
          setCustomToken('');
          pushToast('控制台令牌已轮换，请立即复制保存下方新令牌后再跳转登录', 'warn');
        } catch (err) {
          pushToast(formatErrorMessage(err, '轮换控制台令牌失败'), 'danger');
        } finally {
          if (mountedRef.current) setRotateBusy(false);
        }
      },
    });
  };

  const copyRotatedToken = async () => {
    if (!rotatedToken) return;
    try {
      await navigator.clipboard.writeText(rotatedToken);
      pushToast('已复制新控制台令牌到剪贴板', 'success');
    } catch {
      pushToast('浏览器剪贴板写入被拒绝，请手动选中上方文本复制', 'warn');
    }
  };

  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title={`记忆读取控制 (Memory ACL · ${aclRows.length})`}
            subtitle="管理跨会话或主体对指定目标的记忆读取授权（仅控制 read 范围）"
          />
          <div className="space-y-4 p-4">
            <form onSubmit={(e) => void handleGrantAcl(e)} className="grid gap-2 sm:grid-cols-5">
              <select
                value={aclForm.readerType}
                onChange={(e) => {
                  const v = e.target.value;
                  setAclForm((p) => ({
                    ...p,
                    readerType: v === 'person' || v === 'team' ? v : 'session',
                  }));
                }}
                className={`${INPUT_CLASS} font-mono`}
                aria-label="readerType"
              >
                <option value="session">session</option>
                <option value="person">person</option>
                <option value="team">team</option>
              </select>
              <input
                type="text"
                value={aclForm.readerId}
                onChange={(e) => setAclForm((p) => ({ ...p, readerId: e.target.value }))}
                className={`${INPUT_CLASS} font-mono sm:col-span-2`}
                placeholder="readerId (如 private:10001)"
              />
              <select
                value={aclForm.readType}
                onChange={(e) => {
                  const v = e.target.value;
                  const nextType =
                    v === '*' || v === 'person' || v === 'team' ? v : 'session';
                  setAclForm((p) => ({
                    ...p,
                    readType: nextType,
                    readId: nextType === '*' ? '*' : p.readId === '*' ? '' : p.readId,
                  }));
                }}
                className={`${INPUT_CLASS} font-mono`}
                aria-label="readType"
              >
                <option value="session">session</option>
                <option value="person">person</option>
                <option value="team">team</option>
                <option value="*">* (全部通配)</option>
              </select>
              <input
                type="text"
                disabled={aclForm.readType === '*'}
                value={aclForm.readType === '*' ? '*' : aclForm.readId}
                onChange={(e) => setAclForm((p) => ({ ...p, readId: e.target.value }))}
                className={`${INPUT_CLASS} font-mono`}
                placeholder="目标 readId"
              />
              {wildcardScope && (
                <div className="sm:col-span-5 text-[11px] text-[var(--warn)]">
                  警告：{aclForm.readType === '*' ? 'readType=*' : 'session:*'} 将向该读取方开放全部对话记忆读取权，请谨慎确认。
                </div>
              )}
              <div className="flex items-center justify-between gap-2 sm:col-span-5">
                <span className="text-[11px] text-[var(--text-muted)]">
                  授权级别：read（只读范围）
                </span>
                <Button type="submit" size="sm" variant="primary" disabled={aclBusy}>
                  <UserCheck className="h-3.5 w-3.5" />
                  {aclBusy ? '处理中…' : '添加 ACL 授权'}
                </Button>
              </div>
            </form>

            {aclError ? (
              <EmptyState
                icon={<AlertTriangle className="h-5 w-5 text-[var(--danger)]" />}
                title="记忆 ACL 读取失败"
                description={aclError}
                action={
                  <Button size="sm" onClick={onRetryLoad}>
                    重新加载
                  </Button>
                }
              />
            ) : aclRows.length === 0 ? (
              <div className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3 text-xs text-[var(--text-muted)]">
                暂无额外记忆 ACL 授权条目。
              </div>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full border-collapse text-left text-xs">
                  <thead>
                    <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                      <th className="py-1.5 pr-2 font-medium">读取方</th>
                      <th className="py-1.5 pr-2 font-medium">目标范围</th>
                      <th className="py-1.5 pr-2 font-medium">级别</th>
                      <th className="py-1.5 text-right font-medium">操作</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-[var(--border-hairline)]">
                    {aclRows.map((row) => (
                      <tr key={row.id}>
                        <td className="py-1.5 pr-2 font-mono text-[11px]">
                          {row.reader_type}:{row.reader_id}
                        </td>
                        <td className="py-1.5 pr-2 font-mono text-[11px] text-[var(--text-secondary)]">
                          {row.read_type}:{row.read_id}
                        </td>
                        <td className="py-1.5 pr-2">
                          <Badge tone="neutral">{row.level}</Badge>
                        </td>
                        <td className="py-1.5 text-right">
                          <Button
                            size="sm"
                            variant="ghost"
                            disabled={aclBusy}
                            onClick={() => requestRevokeAcl(row)}
                          >
                            撤销
                          </Button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </Card>

        <Card>
          <CardHeader
            title="会话读取鉴权诊断"
            subtitle="验证指定会话 key 与 agent-token 对是否拥有读取授权 (/api/authorize/read)"
          />
          <form onSubmit={(e) => void handleReadCheck(e)} className="space-y-3 p-4">
            <div>
              <label className="mb-1 block text-[11px] font-medium text-[var(--text-secondary)]">
                会话 Key
              </label>
              <input
                type="text"
                value={readKey}
                onChange={(e) => setReadKey(e.target.value)}
                className={`${INPUT_CLASS} font-mono`}
                placeholder="例如 private:10001 或 group:10001"
              />
            </div>
            <div>
              <label className="mb-1 block text-[11px] font-medium text-[var(--text-secondary)]">
                Agent Token（仅单次诊断使用，不落盘）
              </label>
              <input
                type="password"
                value={readToken}
                onChange={(e) => setReadToken(e.target.value)}
                className={`${INPUT_CLASS} font-mono`}
                placeholder="输入待验证的会话令牌"
              />
            </div>
            <div className="flex items-center justify-between gap-2">
              {readResult ? (
                <Badge tone={readResult.ok ? 'success' : 'danger'}>{readResult.text}</Badge>
              ) : (
                <span className="text-[11px] text-[var(--text-muted)]">仅返回校验结论</span>
              )}
              <Button type="submit" size="sm" disabled={readBusy}>
                <KeyRound className="h-3.5 w-3.5" />
                {readBusy ? '验证中…' : '验证读取权限'}
              </Button>
            </div>
          </form>
        </Card>
      </div>

      <Card danger>
        <CardHeader
          title="危险操作：轮换控制台令牌 (/api/console/token)"
          subtitle="立即吊销所有历史浏览器会话并要求重新登录，且不可通过审计撤销"
        />
        <div className="space-y-3 p-4">
          <div className="flex items-start gap-2 rounded-md border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-xs text-[var(--danger)] dark:border-rose-900">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            <span>
              请勿在不具备备用接入手段时误触此按钮；若留空输入框，后端将自动生成高熵新令牌并写入凭据存储。
            </span>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <input
              type="password"
              value={customToken}
              onChange={(e) => setCustomToken(e.target.value)}
              className={`${INPUT_CLASS} max-w-md font-mono`}
              placeholder="可选：手动指定新控制台令牌（留空则自动生成）"
            />
            <Button
              size="sm"
              variant="danger"
              disabled={rotateBusy}
              onClick={requestRotateToken}
            >
              {rotateBusy ? '轮换中…' : '轮换控制台令牌'}
            </Button>
          </div>

          {rotatedToken && (
            <div className="space-y-2 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] p-3">
              <div className="flex flex-wrap items-center justify-between gap-2 text-xs font-medium text-[var(--text-primary)]">
                <span>新控制台令牌（仅此次明文展示，请复制保存）</span>
                <div className="flex items-center gap-2">
                  <Button size="sm" onClick={() => void copyRotatedToken()}>
                    <Copy className="h-3.5 w-3.5" />
                    复制
                  </Button>
                  <Button
                    size="sm"
                    variant="primary"
                    onClick={() => void refreshAuthAndShell()}
                  >
                    已妥善保存，刷新登录态
                  </Button>
                </div>
              </div>
              <KeyValueRow
                label="Token"
                value={<span className="select-all font-mono">{rotatedToken}</span>}
              />
            </div>
          )}
        </div>
      </Card>
    </div>
  );
};
