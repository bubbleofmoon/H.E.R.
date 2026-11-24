import React, { useEffect, useState } from 'react';
import { Lock } from 'lucide-react';
import { ApiRequestError, submitPasswordLogin } from '../api/client';
import { sanitizeNextPath } from '../catalog/navigation';
import { Button, Card } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';

export const LoginPage: React.FC = () => {
  const {
    authStatus,
    expiredNotice,
    clearExpiredNotice,
    navigate,
    navigatePath,
    nextPathAfterAuth,
    pushToast,
    refreshAuthAndShell,
  } = useConsole();

  const [password, setPassword] = useState('');
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [cooldownSeconds, setCooldownSeconds] = useState(0);
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    const initial = authStatus?.retryAfterSeconds ?? 0;
    if (initial > 0) {
      setCooldownSeconds(initial);
    }
  }, [authStatus?.retryAfterSeconds]);

  useEffect(() => {
    if (cooldownSeconds <= 0) return;
    const timer = window.setInterval(() => {
      setCooldownSeconds((prev) => (prev > 1 ? prev - 1 : 0));
    }, 1000);
    return () => window.clearInterval(timer);
  }, [cooldownSeconds]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!password) {
      setErrorMessage('请输入管理员密码');
      return;
    }
    setSubmitting(true);
    setErrorMessage(null);
    clearExpiredNotice();
    try {
      await submitPasswordLogin(password);
      pushToast('管理员密码验证成功', 'success');
      const queryNext = sanitizeNextPath(new URLSearchParams(window.location.search).get('next'));
      const destination = queryNext ?? sanitizeNextPath(nextPathAfterAuth) ?? '/';
      await refreshAuthAndShell();
      navigatePath(destination);
    } catch (err) {
      if (err instanceof ApiRequestError) {
        if (err.payload.passwordSet === false) {
          navigate('setup');
          return;
        }
        const retry = Number(err.payload.retryAfterSeconds ?? 0);
        if (Number.isFinite(retry) && retry > 0) {
          setCooldownSeconds(Math.ceil(retry));
        }
        setErrorMessage(err.message);
      } else {
        setErrorMessage('登录请求失败，请检查本地控制台连接');
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="flex min-h-full items-center justify-center bg-[var(--bg-canvas)] px-4 pb-16">
      <Card className="w-full max-w-sm p-6 shadow-md">
        <div className="flex items-center gap-2.5">
          <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-[var(--accent-soft)] text-[var(--accent)]">
            <Lock className="h-4 w-4" />
          </div>
          <div>
            <h1 className="text-[17px] font-semibold text-[var(--text-primary)]">HER 控制台登录</h1>
            <p className="text-xs text-[var(--text-muted)]">仅限管理员 · http://her.localhost</p>
          </div>
        </div>

        <form onSubmit={(e) => void handleSubmit(e)} className="mt-5 space-y-3 text-xs">
          {expiredNotice && (
            <div className="rounded-md border border-amber-300 bg-[var(--warn-soft)] p-2.5 text-[var(--warn)] dark:border-amber-900">
              {expiredNotice}
            </div>
          )}

          {cooldownSeconds > 0 && (
            <div className="rounded-md border border-amber-300 bg-[var(--warn-soft)] p-2.5 text-[var(--warn)] dark:border-amber-900">
              尝试次数过多，请在 {cooldownSeconds} 秒后重试（忘记密码可使用{' '}
              <code>her-cli console reset-password</code> 重置）。
            </div>
          )}

          {errorMessage && (
            <div className="rounded-md border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[var(--danger)] dark:border-rose-900">
              {errorMessage}
            </div>
          )}

          <div>
            <label
              htmlFor="login-password"
              className="mb-1 block font-medium text-[var(--text-secondary)]"
            >
              管理员密码
            </label>
            <input
              id="login-password"
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="输入管理员密码…"
              className="h-9 w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-3 text-[13px] text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
            />
          </div>

          <Button
            type="submit"
            variant="primary"
            disabled={submitting || cooldownSeconds > 0}
            className="h-9 w-full"
          >
            {submitting
              ? '正在验证…'
              : cooldownSeconds > 0
                ? `请等待 ${cooldownSeconds} 秒后重试`
                : '登录控制台'}
          </Button>

          {authStatus?.passwordSet === false && (
            <button
              type="button"
              onClick={() => navigate('setup')}
              className="block w-full text-center text-[11px] text-[var(--text-muted)] hover:text-[var(--accent)]"
            >
              尚未设置管理员密码？前往首次设置
            </button>
          )}
        </form>
      </Card>
    </div>
  );
};
