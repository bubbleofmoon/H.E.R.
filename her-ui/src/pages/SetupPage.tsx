import React, { useState } from 'react';
import { KeyRound, ShieldCheck } from 'lucide-react';
import { ApiRequestError, submitFirstRunSetup } from '../api/client';
import { Button, Card } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';

export const SetupPage: React.FC = () => {
  const { authStatus, navigate, navigatePath, pushToast, refreshAuthAndShell } = useConsole();
  const [token, setToken] = useState('');
  const [pwd, setPwd] = useState('');
  const [pwd2, setPwd2] = useState('');
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const tokenFile = authStatus?.tokenFile || 'data/console-token';
  const fromConfig = tokenFile === 'config/config.json';

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!token.trim()) {
      setErrorMessage('请填写初始化控制台令牌');
      return;
    }
    if ([...pwd].length < 8) {
      setErrorMessage('管理员密码至少需要 8 个字符');
      return;
    }
    if (pwd !== pwd2) {
      setErrorMessage('两次输入的密码不一致');
      return;
    }
    setSubmitting(true);
    setErrorMessage(null);
    try {
      await submitFirstRunSetup(token.trim(), pwd);
      pushToast('已完成管理员密码初始化并登录', 'success');
      await refreshAuthAndShell();
      navigatePath('/');
    } catch (err) {
      if (err instanceof ApiRequestError) {
        if (err.status === 409) {
          pushToast('管理员密码已设置，请使用密码登录', 'info');
          navigate('login');
          return;
        }
        setErrorMessage(err.message);
      } else {
        setErrorMessage('初始化请求失败，请检查本地控制台连接');
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="flex min-h-full items-center justify-center bg-[var(--bg-canvas)] px-4 pb-16">
      <Card className="w-full max-w-md p-6 shadow-md">
        <div className="flex items-center gap-2.5">
          <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-[var(--accent-soft)] text-[var(--accent)]">
            <KeyRound className="h-4 w-4" />
          </div>
          <div>
            <h1 className="text-[17px] font-semibold text-[var(--text-primary)]">
              首次设置管理员密码
            </h1>
            <p className="text-xs text-[var(--text-muted)]">仅在未设置密码的首次启动时出现</p>
          </div>
        </div>

        <div className="mt-4 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3 text-xs text-[var(--text-secondary)]">
          {fromConfig ? (
            <>
              请从本机 HER 运行目录下的{' '}
              <code className="font-mono font-semibold">config/config.json</code> 中复制{' '}
              <code className="font-mono font-semibold">consoleToken</code> 字段值，并设置至少 8
              位管理员密码。
            </>
          ) : (
            <>
              请从本机 HER 运行目录下的{' '}
              <code className="font-mono font-semibold">{tokenFile}</code> 复制控制台令牌（若已在{' '}
              <code className="font-mono font-semibold">config/config.json</code> 中设置{' '}
              <code className="font-mono font-semibold">consoleToken</code>{' '}
              则以该字段为准），并设置至少 8 位管理员密码。
            </>
          )}
        </div>

        <form onSubmit={(e) => void handleSubmit(e)} className="mt-4 space-y-3 text-xs">
          {errorMessage && (
            <div className="rounded-md border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[var(--danger)] dark:border-rose-900">
              {errorMessage}
            </div>
          )}

          <div>
            <label
              htmlFor="setup-console-token"
              className="mb-1 block font-medium text-[var(--text-secondary)]"
            >
              控制台令牌
            </label>
            <input
              id="setup-console-token"
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder={
                fromConfig
                  ? '粘贴 config/config.json 中的 consoleToken…'
                  : `粘贴 ${tokenFile} 或 config.json 中的 consoleToken…`
              }
              className="h-9 w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-3 text-[13px] text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
            />
          </div>
          <div>
            <label
              htmlFor="setup-password"
              className="mb-1 block font-medium text-[var(--text-secondary)]"
            >
              新管理员密码（至少 8 位）
            </label>
            <input
              id="setup-password"
              type="password"
              autoComplete="new-password"
              value={pwd}
              onChange={(e) => setPwd(e.target.value)}
              className="h-9 w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-3 text-[13px] text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
            />
          </div>
          <div>
            <label
              htmlFor="setup-password-confirm"
              className="mb-1 block font-medium text-[var(--text-secondary)]"
            >
              确认新密码
            </label>
            <input
              id="setup-password-confirm"
              type="password"
              autoComplete="new-password"
              value={pwd2}
              onChange={(e) => setPwd2(e.target.value)}
              className="h-9 w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-3 text-[13px] text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
            />
          </div>
          <Button type="submit" variant="primary" disabled={submitting} className="h-9 w-full">
            <ShieldCheck className="h-4 w-4" />
            <span>{submitting ? '正在提交…' : '完成设置并进入控制台'}</span>
          </Button>
        </form>
      </Card>
    </div>
  );
};
