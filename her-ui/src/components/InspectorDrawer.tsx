import React, { useEffect, useState } from 'react';
import { X } from 'lucide-react';
import { useConsole } from '../stores/consoleStore';
import { AuditInspector } from './AuditInspector';
import { Badge, Button, SettingField } from './Primitives';
import { ConversationInspector } from './ConversationInspector';

export const InspectorDrawer: React.FC = () => {
  const {
    inspector,
    setInspector,
    settings,
    conversations,
    saveSetting,
    resetSetting,
    openConfirm,
    pushToast,
    setInspectorDirty,
  } = useConsole();

  const [draftValue, setDraftValue] = useState<unknown>(undefined);
  const [busy, setBusy] = useState(false);

  const settingItem =
    inspector?.kind === 'setting'
      ? settings.find((item) => item.key === inspector.key) ?? null
      : null;

  useEffect(() => {
    if (settingItem) {
      setDraftValue(settingItem.value);
    } else {
      setDraftValue(undefined);
    }
  }, [inspector, settingItem]);

  const isSettingDirty =
    settingItem !== null &&
    draftValue !== undefined &&
    JSON.stringify(draftValue) !== JSON.stringify(settingItem.value);

  useEffect(() => {
    if (inspector?.kind === 'setting') {
      setInspectorDirty(isSettingDirty);
      return () => setInspectorDirty(false);
    }
    if (!inspector) {
      setInspectorDirty(false);
    }
    return undefined;
  }, [inspector, isSettingDirty, setInspectorDirty]);

  if (!inspector) return null;

  const handleSaveSetting = async () => {
    if (!settingItem) return;
    const executeSave = async (confirm: boolean) => {
      setBusy(true);
      try {
        await saveSetting(settingItem.key, draftValue, confirm);
      } catch (err) {
        pushToast(err instanceof Error ? err.message : '保存设置失败', 'danger');
      } finally {
        setBusy(false);
      }
    };

    if (settingItem.confirm) {
      openConfirm({
        title: `确认修改安全设置 ${settingItem.key}`,
        description:
          '修改安全允许/拒绝列表、所有者或安全开关前需要显式确认，并在必要时立即吊销失去权限的会话。',
        confirmLabel: '确认保存',
        danger: true,
        onConfirm: () => executeSave(true),
      });
      return;
    }
    await executeSave(false);
  };

  const handleResetSetting = async () => {
    if (!settingItem) return;
    const executeReset = async (confirm: boolean) => {
      setBusy(true);
      try {
        await resetSetting(settingItem.key, confirm);
      } catch (err) {
        pushToast(err instanceof Error ? err.message : '恢复默认值失败', 'danger');
      } finally {
        setBusy(false);
      }
    };

    if (settingItem.confirm) {
      openConfirm({
        title: `确认恢复安全设置 ${settingItem.key} 为默认值`,
        description: '此操作会重置该安全设置并在必要时重新计算访问权限。',
        confirmLabel: '确认重置',
        danger: true,
        onConfirm: () => executeReset(true),
      });
      return;
    }
    await executeReset(false);
  };

  return (
    <aside
      aria-label="右侧详情检视抽屉"
      className="fixed inset-y-0 right-0 z-30 flex w-full max-w-[440px] flex-col border-l border-[var(--border-hairline)] bg-[var(--bg-surface)] shadow-2xl"
    >
      <div className="flex items-start justify-between gap-2 border-b border-[var(--border-hairline)] px-4 py-3">
        <div className="min-w-0">
          {inspector.kind === 'setting' && settingItem ? (
            <>
              <div className="flex items-center gap-2">
                <h3 className="truncate text-[15px] font-semibold text-[var(--text-primary)]">
                  {settingItem.label}
                </h3>
                <Badge tone={settingItem.confirm ? 'warn' : 'accent'}>
                  {settingItem.applies}
                </Badge>
              </div>
              <div className="mt-0.5 font-mono text-[11px] text-[var(--text-muted)]">
                {settingItem.key}
              </div>
            </>
          ) : inspector.kind === 'conversation' ? (
            <>
              <h3 className="truncate text-[15px] font-semibold text-[var(--text-primary)]">
                {conversations.find((c) => c.key === inspector.id)?.name || inspector.id}
              </h3>
              <div className="mt-0.5 font-mono text-[11px] text-[var(--text-muted)]">
                {inspector.id}
              </div>
            </>
          ) : inspector.kind === 'audit' ? (
            <h3 className="truncate text-[15px] font-semibold text-[var(--text-primary)]">
              审计详情 ({inspector.id})
            </h3>
          ) : (
            <h3 className="truncate text-[15px] font-semibold text-[var(--text-primary)]">
              详情检视 ({inspector.kind === 'setting' ? inspector.key : inspector.id})
            </h3>
          )}
        </div>
        <button
          type="button"
          onClick={() => setInspector(null)}
          aria-label="关闭抽屉"
          className="rounded-md p-1 text-[var(--text-muted)] hover:bg-[var(--bg-hover)]"
        >
          <X className="h-4 w-4" />
        </button>
      </div>

      {inspector.kind === 'conversation' ? (
        <ConversationInspector key={inspector.id} conversationKey={inspector.id} />
      ) : (
        <>
          <div className="flex-1 space-y-4 overflow-y-auto p-4 text-xs">
            {inspector.kind === 'setting' && settingItem ? (
              <SettingField
                setting={settingItem}
                draftValue={draftValue}
                onDraftChange={setDraftValue}
                onReset={() => void handleResetSetting()}
              />
            ) : inspector.kind === 'audit' ? (
              <AuditInspector key={inspector.id} auditId={inspector.id} />
            ) : (
              <p className="text-[var(--text-muted)]">详情视图将在对应功能切片中接入。</p>
            )}
          </div>

          <footer className="flex shrink-0 items-center justify-between gap-2 border-t border-[var(--border-hairline)] bg-[var(--bg-subtle)]/60 px-4 py-2.5">
            {inspector.kind === 'setting' &&
            settingItem &&
            settingItem.managedBy !== 'consoleAuth' ? (
              <>
                <span className="text-[11px] text-[var(--text-muted)]">
                  {isSettingDirty ? '有未保存的更改' : '与服务端同步'}
                </span>
                <div className="flex items-center gap-2">
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={!isSettingDirty || busy}
                    onClick={() => setDraftValue(settingItem.value)}
                  >
                    放弃
                  </Button>
                  <Button
                    size="sm"
                    variant="primary"
                    disabled={!isSettingDirty || busy}
                    onClick={() => void handleSaveSetting()}
                  >
                    保存
                  </Button>
                </div>
              </>
            ) : (
              <div className="ml-auto">
                <Button size="sm" variant="secondary" onClick={() => setInspector(null)}>
                  关闭
                </Button>
              </div>
            )}
          </footer>
        </>
      )}
    </aside>
  );
};
