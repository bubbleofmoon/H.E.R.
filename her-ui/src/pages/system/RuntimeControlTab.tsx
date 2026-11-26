import React, { useRef, useState } from 'react';
import { AlertTriangle, Power, RotateCcw, Send } from 'lucide-react';
import {
  DesiredView,
  HealthResponse,
  QqLoginResponse,
  optionalTestSend,
  optionalWorkspaceReset,
  queueRestart,
  setLifecycleState,
} from '../../api/system';
import { Badge, Button, Card, CardHeader } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatErrorMessage } from '../../utils/systemFormat';

interface RuntimeControlTabProps {
  busy: boolean;
  setBusy: React.Dispatch<React.SetStateAction<boolean>>;
  desired: DesiredView | null;
  health: HealthResponse | null;
  qqLogin: QqLoginResponse | null;
  qqLoginState: string;
  unsupportedNotice: string | null;
  setUnsupportedNotice: React.Dispatch<React.SetStateAction<string | null>>;
  mountedRef: React.MutableRefObject<boolean>;
  onRefreshAll: () => Promise<void>;
}

export const RuntimeControlTab: React.FC<RuntimeControlTabProps> = ({
  busy,
  setBusy,
  desired,
  health,
  qqLogin,
  qqLoginState,
  setUnsupportedNotice,
  mountedRef,
  onRefreshAll,
}) => {
  const { openConfirm, pushToast, triggerQqLoginWindow, refreshLiveData } = useConsole();

  const [maintMinutes, setMaintMinutes] = useState('30');
  const [lifecycleReason, setLifecycleReason] = useState('');
  const [testKind, setTestKind] = useState<'group' | 'private'>('private');
  const [testTargetId, setTestTargetId] = useState('');
  const [testMessage, setTestMessage] = useState('');
  const testMsgGenRef = useRef(0);

  const effectiveQqState = qqLoginState || 'unknown';

  const handleLifecycle = (targetState: 'running' | 'stopped' | 'maintenance') => {
    if (busy) return;
    let minutes: number | undefined;
    if (targetState === 'maintenance') {
      const trimmed = maintMinutes.trim();
      if (trimmed) {
        const n = Number(trimmed);
        if (!Number.isInteger(n) || n < 1 || n > 525600) {
          pushToast('维护时长需为 1-525600 之间的整数分钟', 'warn');
          return;
        }
        minutes = n;
      }
    }

    openConfirm({
      title: `确认将监督进程目标状态切换为「${targetState}」？`,
      description:
        targetState === 'running'
          ? '将允许 live 桥接、控制台与全部定时任务正常运行。'
          : targetState === 'maintenance'
            ? '维护模式将停止 live 与后台定时任务，仅保留控制台用于恢复或维护。'
            : '停止模式将向监督进程请求暂停服务（无运行中监督进程时将返回 503）。',
      confirmLabel: '确认切换',
      danger: targetState !== 'running',
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await setLifecycleState({
            state: targetState,
            minutes,
            reason: lifecycleReason.trim(),
            confirm: true,
          });
          pushToast(
            res.applied === false
              ? `已提交目标状态 ${targetState}（等待监督进程应用）`
              : `监督进程目标状态已更新为 ${targetState}`,
            'success',
          );
          await Promise.all([
            mountedRef.current ? onRefreshAll() : Promise.resolve(),
            refreshLiveData(),
          ]);
        } catch (err) {
          pushToast(formatErrorMessage(err, '切换监督进程状态失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleQueueRestart = () => {
    openConfirm({
      title: '确认重启控制台进程？',
      description:
        '将通过 /api/restart 提交重启请求；监督进程将停止当前控制台进程并拉起新控制台实例（此操作仅重启控制台，不直接重启独立的 live QQ 桥接进程）。',
      confirmLabel: '确认重启控制台',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await queueRestart();
          pushToast(`控制台重启请求已排队（requestId=${res.requestId}）`, 'success');
        } catch (err) {
          pushToast(formatErrorMessage(err, '触发重启失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleWorkspaceReset = () => {
    openConfirm({
      title: '确认重置 QQ 聊天工作区？',
      description:
        '此操作针对工作区状态具有破坏性；若当前隔离运行时未挂载该接口，将安全返回未就绪提示。',
      confirmLabel: '确认重置工作区',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await optionalWorkspaceReset();
          if (!mountedRef.current) return;
          if (res.available) {
            pushToast('已提交工作区重置请求', 'success');
          } else {
            setUnsupportedNotice(
              `工作区重置接口在当前隔离运行时暂不可用（HTTP ${res.status}）：${res.reason}`,
            );
            pushToast('当前隔离运行时未启用工作区重置接口', 'warn');
          }
        } catch (err) {
          pushToast(formatErrorMessage(err, '重置工作区失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const handleTestSend = async () => {
    if (busy) return;
    const cleanId = Number(testTargetId.trim());
    if (!Number.isInteger(cleanId) || cleanId <= 0) {
      pushToast('请输入有效的正整数目标群号或 QQ 号', 'warn');
      return;
    }
    const msg = testMessage.trim();
    if (!msg) {
      pushToast('请输入测试消息文本', 'warn');
      return;
    }
    const startMsgGen = testMsgGenRef.current;
    setBusy(true);
    try {
      const res = await optionalTestSend({ kind: testKind, id: cleanId, message: msg });
      if (!mountedRef.current) return;
      if (res.available) {
        pushToast('测试消息已发送', 'success');
        if (testMsgGenRef.current === startMsgGen) {
          setTestMessage('');
        }
      } else {
        setUnsupportedNotice(
          `本地测试发消息路由 /api/test-send 在当下隔离运行环境无法连通（状态码 ${res.status}）：${res.reason}`,
        );
        pushToast('当前隔离运行时未挂载 /api/test-send', 'warn');
      }
    } catch (err) {
      pushToast(formatErrorMessage(err, '测试发送失败'), 'danger');
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  };

  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title={
              <span className="inline-flex items-center gap-1.5">
                <Power className="h-4 w-4 text-[var(--accent)]" />
                监督进程运行状态控制
              </span>
            }
            subtitle="查看实际健康/就绪状态并切换运行 / 维护 / 停止目标"
            actions={
              health && (
                <Badge tone={health.ok ? 'success' : 'warn'}>
                  {health.ok ? '健康就绪' : '恢复/降级中'}
                </Badge>
              )
            }
          />
          <div className="space-y-3 p-4 text-xs">
            <div className="rounded bg-[var(--bg-subtle)]/60 p-2.5 space-y-1">
              <div>
                期望目标状态：<strong className="font-mono">{desired?.state || '—'}</strong>
                {health?.pid !== undefined && (
                  <span className="ml-2 font-mono text-[11px] text-[var(--text-muted)]">
                    PID {health.pid}
                  </span>
                )}
              </div>
              {desired && 'wants' in desired && (
                <div className="text-[11px] text-[var(--text-muted)]">
                  live={String(desired.wants.live)} · console={String(desired.wants.console)} · jobs=
                  {String(desired.wants.jobs)}
                </div>
              )}
              {desired && 'reason' in desired && desired.state === 'unknown' && (
                <div className="text-[11px] text-[var(--warn)]">{desired.reason}</div>
              )}
              {health && !health.ok && (
                <div className="text-[11px] text-[var(--danger)]">
                  实测健康检查未就绪（administratorReady={String(health.administratorReady)}）：
                  {health.recoveryReason || '请排查恢复状态'}
                </div>
              )}
            </div>

            <div className="grid gap-2 sm:grid-cols-2">
              <label className="space-y-1">
                <span className="block text-[var(--text-secondary)]">维护时长（分钟，可选）</span>
                <input
                  type="number"
                  min={1}
                  max={525600}
                  value={maintMinutes}
                  onChange={(e) => setMaintMinutes(e.target.value)}
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
                />
              </label>
              <label className="space-y-1">
                <span className="block text-[var(--text-secondary)]">操作原因说明（可选）</span>
                <input
                  type="text"
                  maxLength={300}
                  value={lifecycleReason}
                  onChange={(e) => setLifecycleReason(e.target.value)}
                  placeholder="例如：例行维护 / 配置调整"
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
                />
              </label>
            </div>

            <div className="flex flex-wrap gap-2 pt-1">
              <Button
                size="sm"
                variant="primary"
                disabled={busy}
                onClick={() => handleLifecycle('running')}
              >
                恢复正常运行
              </Button>
              <Button
                size="sm"
                variant="secondary"
                disabled={busy}
                onClick={() => handleLifecycle('maintenance')}
              >
                进入维护模式
              </Button>
              <Button
                size="sm"
                variant="danger"
                disabled={busy}
                onClick={() => handleLifecycle('stopped')}
              >
                请求停止服务
              </Button>
            </div>
          </div>
        </Card>

        <Card>
          <CardHeader
            title="QQ 登录探针与消息测试"
            subtitle="探测托管运行时的 QQ 登录窗口状态与连通性"
          />
          <div className="space-y-4 p-4 text-xs">
            <div className="flex items-center justify-between rounded bg-[var(--bg-subtle)]/60 p-2.5">
              <div>
                <div className="text-[var(--text-muted)]">当前 QQ 登录状态</div>
                <div className="mt-0.5 font-medium text-[var(--text-primary)]">
                  {effectiveQqState === 'logged-in'
                    ? '已登录'
                    : effectiveQqState === 'needs-login'
                      ? `等待登录（进程数 ${qqLogin?.processes.length ?? 0}）`
                      : '状态未知'}
                </div>
              </div>
              {effectiveQqState === 'needs-login' && (
                <Button size="sm" variant="primary" onClick={triggerQqLoginWindow}>
                  打开登录窗口
                </Button>
              )}
            </div>

            <div className="space-y-2 border-t border-[var(--border-hairline)] pt-3">
              <div className="font-medium text-[var(--text-primary)]">
                测试发送消息 (/api/test-send)
              </div>
              <div className="grid gap-2 sm:grid-cols-[120px_1fr]">
                <select
                  value={testKind}
                  onChange={(e) =>
                    setTestKind(e.target.value === 'group' ? 'group' : 'private')
                  }
                  className="h-8 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
                >
                  <option value="private">私聊</option>
                  <option value="group">群聊</option>
                </select>
                <input
                  type="text"
                  value={testTargetId}
                  onChange={(e) => setTestTargetId(e.target.value)}
                  placeholder="输入白名单内的群号或 QQ 号…"
                  className="h-8 w-full rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 font-mono text-xs"
                />
              </div>
              <div className="flex gap-2">
                <input
                  type="text"
                  value={testMessage}
                  onChange={(e) => {
                    testMsgGenRef.current += 1;
                    setTestMessage(e.target.value);
                  }}
                  placeholder="测试文本…"
                  className="h-8 flex-1 rounded border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs"
                />
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={busy}
                  onClick={() => void handleTestSend()}
                >
                  <Send className="h-3.5 w-3.5" />
                  发送测试
                </Button>
              </div>
            </div>
          </div>
        </Card>
      </div>

      <Card danger className="p-4 space-y-2.5 text-xs">
        <div className="flex items-center gap-1.5 font-semibold text-[var(--danger)]">
          <AlertTriangle className="h-4 w-4" />
          <span>危险操作</span>
        </div>
        <p className="text-[var(--text-secondary)]">
          所有破坏性维护操作均需二次确认。重启控制台会停止并由监督进程拉起新控制台进程；工作区重置在当前隔离运行时暂未开放时会保留现场并提示。
        </p>
        <div className="flex flex-wrap gap-2 pt-1">
          <Button size="sm" variant="danger" disabled={busy} onClick={handleQueueRestart}>
            <RotateCcw className="h-3.5 w-3.5" />
            重启控制台进程
          </Button>
          <Button size="sm" variant="danger" disabled={busy} onClick={handleWorkspaceReset}>
            重置 QQ 聊天工作区
          </Button>
        </div>
      </Card>
    </div>
  );
};
