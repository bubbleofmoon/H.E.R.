import React from 'react';
import { AlertTriangle } from 'lucide-react';
import { EmptyState } from '../../components/ConsoleWidgets';
import { Button, Card, CardHeader } from '../../components/Primitives';

export interface WhitelistDraft {
  allowPrivate: string;
  allowGroups: string;
  denyPrivate: string;
  denyGroups: string;
  ownerQQ: string;
}

const INPUT_CLASS =
  'w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-1.5 text-xs font-mono text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50';

const TEXTAREA_CLASS = `${INPUT_CLASS} leading-relaxed`;

interface WhitelistCardProps {
  ready: boolean;
  error: string | null;
  ignoredCount?: number;
  draft: WhitelistDraft;
  dirty: boolean;
  saving: boolean;
  onUpdateField: (field: keyof WhitelistDraft, value: string) => void;
  onSave: () => void;
  onDiscard: () => void;
  onRetry: () => void;
}

export const WhitelistCard: React.FC<WhitelistCardProps> = ({
  ready,
  error,
  ignoredCount = 0,
  draft,
  dirty,
  saving,
  onUpdateField,
  onSave,
  onDiscard,
  onRetry,
}) => (
  <Card>
    <CardHeader
      title="准入黑白名单与主人账号"
      subtitle="每行或逗号分隔填写纯数字 QQ 号/群号（无前导零）；仅逐项提交本次修改过的列表或主人字段"
      actions={
        <div className="flex items-center gap-2">
          {dirty && (
            <Button size="sm" variant="ghost" onClick={onDiscard} disabled={saving}>
              放弃更改
            </Button>
          )}
          <Button
            size="sm"
            variant="primary"
            disabled={!ready || !dirty || saving}
            onClick={onSave}
          >
            {saving ? '保存中…' : '保存白名单'}
          </Button>
        </div>
      }
    />
        <div className="space-y-4 p-4">
          {error && !ready ? (
            <EmptyState
              icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
              title="白名单读取失败"
              description={error}
              action={
                <Button size="sm" onClick={onRetry}>
                  重新加载
                </Button>
              }
            />
          ) : (
            <>
              {error && (
                <div className="flex items-center justify-between gap-2 rounded-md border border-amber-300 bg-[var(--warn-soft)] px-3 py-2 text-[11px] text-[var(--warn)] dark:border-amber-900">
                  <span>最近刷新失败，当前显示缓存名单：{error}</span>
                  <Button size="sm" variant="ghost" onClick={onRetry}>
                    重试
                  </Button>
                </div>
              )}
              {ignoredCount > 0 && (
                <div className="rounded-md border border-amber-300 bg-[var(--warn-soft)] px-3 py-2 text-[11px] text-[var(--warn)] dark:border-amber-900">
                  配置文件中有 {ignoredCount} 条被服务端忽略的无效白名单条目（例如含逗号或前导零），已过滤以免保存对应列表时意外激活。
                </div>
              )}
          <div className="max-w-sm">
            <label className="mb-1 block text-xs font-medium text-[var(--text-primary)]">
              主人 QQ 号 (ownerQQ)
            </label>
            <input
              type="text"
              disabled={!ready || saving}
              value={draft.ownerQQ}
              onChange={(e) => onUpdateField('ownerQQ', e.target.value)}
              className={INPUT_CLASS}
              placeholder={ready ? '留空表示取消绑定主人 QQ' : '加载中…'}
            />
          </div>

          <div className="grid gap-4 md:grid-cols-2">
            <div>
              <label className="mb-1 block text-xs font-medium text-[var(--text-primary)]">
                允许私聊列表 (allow.private)
              </label>
              <textarea
                rows={4}
                disabled={!ready || saving}
                value={draft.allowPrivate}
                onChange={(e) => onUpdateField('allowPrivate', e.target.value)}
                className={TEXTAREA_CLASS}
                placeholder="每行一个 QQ 号"
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-[var(--text-primary)]">
                允许群聊列表 (allow.groups)
              </label>
              <textarea
                rows={4}
                disabled={!ready || saving}
                value={draft.allowGroups}
                onChange={(e) => onUpdateField('allowGroups', e.target.value)}
                className={TEXTAREA_CLASS}
                placeholder="每行一个群号"
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-[var(--text-primary)]">
                拒绝私聊黑名单 (deny.private)
              </label>
              <textarea
                rows={3}
                disabled={!ready || saving}
                value={draft.denyPrivate}
                onChange={(e) => onUpdateField('denyPrivate', e.target.value)}
                className={TEXTAREA_CLASS}
                placeholder="每行一个拦截 QQ 号"
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-[var(--text-primary)]">
                拒绝群聊黑名单 (deny.groups)
              </label>
              <textarea
                rows={3}
                disabled={!ready || saving}
                value={draft.denyGroups}
                onChange={(e) => onUpdateField('denyGroups', e.target.value)}
                className={TEXTAREA_CLASS}
                placeholder="每行一个拦截群号"
              />
            </div>
          </div>
        </>
      )}
    </div>
  </Card>
);
