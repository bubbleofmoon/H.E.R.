import React, { useState } from 'react';
import { AlertTriangle, Check, Info, RotateCcw, X } from 'lucide-react';
import { SettingItemView } from '../api/client';
import { useConsole } from '../stores/consoleStore';

export type BadgeTone = 'neutral' | 'accent' | 'success' | 'warn' | 'danger';

const BADGE_CLASSES: Record<BadgeTone, string> = {
  neutral: 'bg-[var(--bg-subtle)] text-[var(--text-secondary)] border-[var(--border-hairline)]',
  accent: 'bg-[var(--accent-soft)] text-[var(--accent)] border-blue-200 dark:border-blue-900',
  success: 'bg-[var(--success-soft)] text-[var(--success)] border-emerald-200 dark:border-emerald-900',
  warn: 'bg-[var(--warn-soft)] text-[var(--warn)] border-amber-200 dark:border-amber-900',
  danger: 'bg-[var(--danger-soft)] text-[var(--danger)] border-rose-200 dark:border-rose-900',
};

export const Badge: React.FC<{
  tone?: BadgeTone;
  children: React.ReactNode;
  className?: string;
}> = ({ tone = 'neutral', children, className = '' }) => (
  <span
    className={`inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-[11px] font-medium leading-4 whitespace-nowrap ${BADGE_CLASSES[tone]} ${className}`}
  >
    {children}
  </span>
);

export const Button: React.FC<
  React.ButtonHTMLAttributes<HTMLButtonElement> & {
    variant?: 'primary' | 'secondary' | 'ghost' | 'danger';
    size?: 'sm' | 'md';
  }
> = ({ variant = 'secondary', size = 'md', className = '', children, ...rest }) => {
  const base =
    'inline-flex items-center justify-center gap-1.5 rounded-md font-medium transition-colors disabled:opacity-45 disabled:pointer-events-none whitespace-nowrap';
  const sizing = size === 'sm' ? 'h-7 px-2.5 text-xs' : 'h-8 px-3 text-[13px]';
  const variants = {
    primary: 'accent-bg border border-transparent shadow-xs',
    secondary:
      'bg-[var(--bg-surface)] text-[var(--text-primary)] border border-[var(--border-strong)] hover:bg-[var(--bg-hover)]',
    ghost:
      'bg-transparent text-[var(--text-secondary)] border border-transparent hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]',
    danger: 'bg-[var(--danger)] text-white border border-transparent hover:opacity-90',
  };
  return (
    <button type="button" className={`${base} ${sizing} ${variants[variant]} ${className}`} {...rest}>
      {children}
    </button>
  );
};

export const Switch: React.FC<{
  checked: boolean;
  onChange: (next: boolean) => void;
  label?: string;
  disabled?: boolean;
}> = ({ checked, onChange, label, disabled }) => (
  <button
    type="button"
    role="switch"
    aria-checked={checked}
    aria-label={label}
    disabled={disabled}
    onClick={() => onChange(!checked)}
    className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full border transition-colors ${
      checked
        ? 'border-transparent bg-[var(--accent)]'
        : 'border-[var(--border-strong)] bg-[var(--bg-subtle)]'
    } ${disabled ? 'opacity-50' : ''}`}
  >
    <span
      className={`inline-block h-3.5 w-3.5 rounded-full bg-white shadow-xs transition-transform ${
        checked ? 'translate-x-4' : 'translate-x-0.5'
      }`}
    />
  </button>
);

export const Card: React.FC<{
  children: React.ReactNode;
  className?: string;
  danger?: boolean;
}> = ({ children, className = '', danger = false }) => (
  <section
    className={`rounded-lg border ${
      danger
        ? 'border-rose-300 bg-[var(--danger-soft)]/35 dark:border-rose-900/70'
        : 'border-[var(--border-hairline)] bg-[var(--bg-surface)]'
    } ${className}`}
  >
    {children}
  </section>
);

export const CardHeader: React.FC<{
  title: React.ReactNode;
  subtitle?: React.ReactNode;
  actions?: React.ReactNode;
}> = ({ title, subtitle, actions }) => (
  <div className="flex flex-wrap items-center justify-between gap-2 border-b border-[var(--border-hairline)] px-4 py-2.5">
    <div className="min-w-0">
      <h2 className="text-[14px] font-semibold text-[var(--text-primary)]">{title}</h2>
      {subtitle && <p className="mt-0.5 text-xs text-[var(--text-muted)]">{subtitle}</p>}
    </div>
    {actions && <div className="flex items-center gap-2">{actions}</div>}
  </div>
);

export const PageHeader: React.FC<{
  title: string;
  description?: string;
  actions?: React.ReactNode;
  badge?: React.ReactNode;
}> = ({ title, description, actions, badge }) => (
  <div className="mb-4 flex flex-wrap items-start justify-between gap-3">
    <div className="min-w-0">
      <div className="flex items-center gap-2">
        <h1 className="text-[19px] font-semibold tracking-tight text-[var(--text-primary)]">
          {title}
        </h1>
        {badge}
      </div>
      {description && <p className="mt-0.5 text-[12.5px] text-[var(--text-muted)]">{description}</p>}
    </div>
    {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
  </div>
);

export const Tabs: React.FC<{
  tabs: { id: string; label: string; count?: number | string }[];
  active: string;
  onChange: (id: string) => void;
  className?: string;
}> = ({ tabs, active, onChange, className = '' }) => (
  <div
    className={`mb-4 flex items-center gap-1 overflow-x-auto border-b border-[var(--border-hairline)] ${className}`}
    role="tablist"
  >
    {tabs.map((tab) => {
      const isActive = tab.id === active;
      return (
        <button
          key={tab.id}
          type="button"
          role="tab"
          aria-selected={isActive}
          onClick={() => onChange(tab.id)}
          className={`relative -mb-px flex items-center gap-1.5 whitespace-nowrap border-b-2 px-3 py-2 text-[13px] font-medium transition-colors ${
            isActive
              ? 'border-[var(--accent)] text-[var(--accent)]'
              : 'border-transparent text-[var(--text-secondary)] hover:text-[var(--text-primary)]'
          }`}
        >
          <span>{tab.label}</span>
          {tab.count !== undefined && (
            <span className="rounded bg-[var(--bg-subtle)] px-1.5 py-0.2 text-[10px] text-[var(--text-muted)]">
              {tab.count}
            </span>
          )}
        </button>
      );
    })}
  </div>
);

function formatSettingDisplay(item: SettingItemView): string {
  if (item.kind.type === 'password') {
    if (item.isSet) return '已设置（设置库绑定）';
    if (item.source === 'file') return '已设置（配置文件兜底）';
    if (item.boundElsewhere) return '端点已变更，需重新输入';
    return '未设置';
  }
  if (item.value === null || item.value === undefined) {
    if (
      item.kind.type === 'model endpoint' &&
      typeof item.fallback?.value === 'string' &&
      item.fallback.value
    ) {
      return `跟随默认：${item.fallback.value}`;
    }
    return '（未配置）';
  }
  if (typeof item.value === 'string') return item.value;
  return JSON.stringify(item.value);
}

export const SettingField: React.FC<{
  setting: SettingItemView;
  draftValue?: unknown;
  onDraftChange?: (value: unknown) => void;
  onReset?: () => void;
}> = ({ setting, draftValue, onDraftChange, onReset }) => {
  const effective = draftValue !== undefined ? draftValue : setting.value;
  const readOnly = setting.managedBy === 'consoleAuth';
  const kind = setting.kind;

  if (readOnly) {
    return (
      <div className="space-y-1.5">
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs font-medium text-[var(--text-primary)]">{setting.label}</span>
          <Badge tone={setting.isSet ? 'success' : 'warn'}>
            {setting.isSet ? '已设置' : '未设置'}
          </Badge>
        </div>
        <div className="font-mono text-[11px] text-[var(--text-muted)]">{setting.key}</div>
        <p className="text-xs text-[var(--text-secondary)]">{setting.hint}</p>
      </div>
    );
  }

  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs font-medium text-[var(--text-primary)]">{setting.label}</span>
        {onReset && (
          <button
            type="button"
            onClick={onReset}
            className="inline-flex items-center gap-1 text-[11px] text-[var(--text-muted)] hover:text-[var(--accent)]"
          >
            <RotateCcw className="h-3 w-3" />
            恢复默认
          </button>
        )}
      </div>
      <div className="font-mono text-[11px] text-[var(--text-muted)]">{setting.key}</div>

      {kind.type === 'switch' ? (
        <div className="flex items-center gap-2 pt-0.5">
          <Switch
            checked={effective === true}
            onChange={(next) => onDraftChange?.(next)}
            label={setting.label}
          />
          <span className="text-xs text-[var(--text-secondary)]">
            {effective === true ? '开启' : '关闭'}
          </span>
        </div>
      ) : kind.type === 'text' || kind.type === 'route list' ? (
        <textarea
          rows={5}
          value={
            kind.type === 'route list'
              ? draftValue === undefined && setting.redacted
                ? ''
                : typeof effective === 'string'
                  ? effective
                  : effective
                    ? JSON.stringify(effective, null, 2)
                    : ''
              : typeof effective === 'string'
                ? effective
                : ''
          }
          onChange={(e) => {
            onDraftChange?.(e.target.value);
          }}
          placeholder={
            setting.redacted
              ? '当前值已脱敏，输入完整 JSON 数组以整体替换（不可包含 [REDACTED]）'
              : kind.type === 'route list'
                ? '[{"id": "primary", "model": "..."}]'
                : ''
          }
          className="w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-2 font-mono text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
        />
      ) : kind.type === 'password' ? (
        <input
          type="password"
          autoComplete="new-password"
          value={typeof draftValue === 'string' ? draftValue : ''}
          onChange={(e) => onDraftChange?.(e.target.value)}
          placeholder={
            setting.isSet
              ? '已设置（输入新值以更新）'
              : setting.source === 'file'
                ? '已设置（配置文件兜底，输入新值以覆盖）'
                : setting.boundElsewhere
                  ? '端点已变更，请重新输入凭据…'
                  : '输入凭据…'
          }
          className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
        />
      ) : kind.type === 'model endpoint' ? (
        <div className="space-y-1">
          <input
            type="text"
            maxLength={kind.maxChars}
            value={typeof effective === 'string' ? effective : ''}
            onChange={(e) => {
              onDraftChange?.(e.target.value);
            }}
            placeholder={
              typeof setting.fallback?.value === 'string' && setting.fallback.value
                ? `跟随默认：${setting.fallback.value}`
                : 'https://api.example.com/v1 或 http://127.0.0.1:8317'
            }
            className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none"
          />
          <div className="flex flex-wrap items-center gap-2 text-[11px] text-[var(--text-muted)]">
            {setting.value === null && typeof setting.fallback?.value === 'string' && (
              <Badge tone="neutral">
                当前生效：跟随默认 ({setting.fallback.source || 'dependencies.json'})
              </Badge>
            )}
            {typeof kind.maxChars === 'number' && <span>最多 {kind.maxChars} 字符</span>}
          </div>
        </div>
      ) : kind.type === 'integer' || kind.type === 'number' || kind.type === 'probability' ? (
        <input
          type="number"
          min={kind.min}
          max={kind.max}
          step={kind.type === 'integer' ? 1 : 'any'}
          value={effective === null || effective === undefined ? '' : String(effective)}
          onChange={(e) => {
            if (!onDraftChange) return;
            const raw = e.target.value.trim();
            onDraftChange(raw === '' ? null : Number(raw));
          }}
          className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
        />
      ) : kind.type === 'choice' && Array.isArray(kind.options) ? (
        <select
          value={typeof effective === 'string' ? effective : ''}
          onChange={(e) => onDraftChange?.(e.target.value)}
          className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
        >
          {typeof effective !== 'string' && <option value="">（请选择）</option>}
          {kind.options.map((opt) => (
            <option key={opt} value={opt}>
              {opt}
            </option>
          ))}
        </select>
      ) : kind.type === 'daily time' ? (
        <input
          type="time"
          value={typeof effective === 'string' ? effective : ''}
          onChange={(e) => onDraftChange?.(e.target.value)}
          className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
        />
      ) : (
        <input
          type="text"
          value={
            (kind.type === 'id list' || kind.type === 'conversation list') &&
            typeof draftValue === 'string'
              ? draftValue
              : Array.isArray(effective)
                ? effective.join(', ')
                : effective === null || effective === undefined
                  ? ''
                  : String(effective)
          }
          onChange={(e) => {
            onDraftChange?.(e.target.value);
          }}
          placeholder={formatSettingDisplay(setting)}
          className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
        />
      )}

      <p className="text-[11.5px] leading-relaxed text-[var(--text-muted)]">{setting.hint}</p>
    </div>
  );
};

export const StickySaveBar: React.FC = () => {
  const { dirtyEntry } = useConsole();
  const [busy, setBusy] = useState(false);
  if (!dirtyEntry || dirtyEntry.count <= 0) return null;

  const handleSave = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await dirtyEntry.onSave();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-x-0 bottom-0 z-30 border-t border-[var(--border-strong)] bg-[var(--bg-surface)]/95 px-4 py-2.5 shadow-lg backdrop-blur">
      <div className="mx-auto flex max-w-6xl items-center justify-between gap-3">
        <div className="text-xs font-medium text-[var(--text-primary)]">
          {dirtyEntry.label || `有 ${dirtyEntry.count} 项未保存更改`}
        </div>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="ghost" disabled={busy} onClick={dirtyEntry.onDiscard}>
            放弃
          </Button>
          <Button size="sm" variant="primary" disabled={busy} onClick={() => void handleSave()}>
            {busy ? '保存中…' : '保存'}
          </Button>
        </div>
      </div>
    </div>
  );
};

export const ToastHost: React.FC = () => {
  const { toasts, dismissToast } = useConsole();
  if (!toasts.length) return null;

  return (
    <div
      className="fixed bottom-4 right-4 z-50 flex max-w-sm flex-col gap-2"
      aria-live="polite"
    >
      {toasts.map((toast) => (
        <div
          key={toast.id}
          className="flex items-center justify-between gap-3 rounded-lg border border-[var(--border-strong)] bg-[var(--bg-surface)] px-3.5 py-2.5 text-xs text-[var(--text-primary)] shadow-xl"
        >
          <div className="flex items-center gap-2">
            {toast.tone === 'success' ? (
              <Check className="h-3.5 w-3.5 shrink-0 text-[var(--success)]" />
            ) : toast.tone === 'warn' || toast.tone === 'danger' ? (
              <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-[var(--warn)]" />
            ) : (
              <Info className="h-3.5 w-3.5 shrink-0 text-[var(--accent)]" />
            )}
            <span>{toast.message}</span>
          </div>
          <div className="flex items-center gap-1.5">
            {toast.onUndo && (
              <button
                type="button"
                onClick={() => {
                  toast.onUndo?.();
                  dismissToast(toast.id);
                }}
                className="font-semibold text-[var(--accent)] hover:underline"
              >
                {toast.undoLabel || '撤销'}
              </button>
            )}
            <button
              type="button"
              onClick={() => dismissToast(toast.id)}
              aria-label="关闭通知"
              className="rounded p-0.5 text-[var(--text-muted)] hover:bg-[var(--bg-hover)]"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      ))}
    </div>
  );
};

export const ConfirmModal: React.FC = () => {
  const { confirmRequest, closeConfirm } = useConsole();
  const [busy, setBusy] = useState(false);

  if (!confirmRequest) return null;

  const runConfirm = async () => {
    setBusy(true);
    try {
      await confirmRequest.onConfirm();
      closeConfirm();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
    >
      <Card className="w-full max-w-md p-5 shadow-2xl">
        <div className="flex items-start gap-3">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-[var(--danger-soft)] text-[var(--danger)]">
            <AlertTriangle className="h-4 w-4" />
          </div>
          <div className="min-w-0 flex-1">
            <h3 className="text-[15px] font-semibold text-[var(--text-primary)]">
              {confirmRequest.title}
            </h3>
            <p className="mt-1.5 whitespace-pre-line text-xs leading-relaxed text-[var(--text-secondary)]">
              {confirmRequest.description}
            </p>
          </div>
        </div>
        <div className="mt-5 flex justify-end gap-2">
          <Button size="sm" variant="secondary" disabled={busy} onClick={closeConfirm}>
            取消
          </Button>
          <Button
            size="sm"
            variant={confirmRequest.danger ? 'danger' : 'primary'}
            disabled={busy}
            onClick={() => void runConfirm()}
          >
            {confirmRequest.confirmLabel || '确认执行'}
          </Button>
        </div>
      </Card>
    </div>
  );
};

export const QqLoginBar: React.FC = () => {
  const { qqLoginState, triggerQqLoginWindow } = useConsole();
  if (qqLoginState !== 'needs-login') return null;

  return (
    <div className="flex items-center justify-between gap-3 border-b border-amber-300 bg-[var(--warn-soft)] px-4 py-1.5 text-xs text-[var(--warn)] dark:border-amber-900">
      <div className="flex items-center gap-2 font-medium">
        <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
        <span>QQ 当前需要重新扫码登录（不影响控制台其他配置）</span>
      </div>
      <Button size="sm" variant="primary" onClick={triggerQqLoginWindow}>
        打开 QQ 登录窗口
      </Button>
    </div>
  );
};
