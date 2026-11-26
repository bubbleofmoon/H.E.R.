import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, ChevronDown, ChevronRight, RefreshCw, Sliders } from 'lucide-react';
import { SettingItemView } from '../../api/client';
import {
  fetchPresetsOptional,
  fetchSocialV2Activity,
  fetchStatusDetail,
  PresetOption,
  resetSocialV2,
  saveMode,
  saveRoleMode,
  setSocialV2Activity,
  StatusDetail,
} from '../../api/config';
import { EmptyState, SearchInput, SegmentedControl } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader, SettingField, Switch } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatAppliesLabel, formatErrorMessage } from '../../utils/format';
import {
  ExtraDirtySource,
  INPUT_CLASS,
  localizeSettingItem,
  settingValuesEqual,
  useSettingsDrafts,
} from '../../utils/herHelpers';

const MODE_OPTIONS: { value: string; label: string }[] = [
  { value: 'reserved2', label: '二代仿真 (reserved2 / simulation)' },
  { value: 'chat', label: '普通聊天 (chat)' },
  { value: 'reserved', label: '一代仿真 (reserved)' },
  { value: 'closed-agent', label: '管理员代理 (closed-agent)' },
];

function normalizeModeValue(raw?: string | null): string {
  if (!raw) return 'reserved2';
  const trimmed = raw.trim();
  if (trimmed === 'simulation') return 'reserved2';
  return trimmed;
}

function isAdvancedBehaviorSetting(item: SettingItemView): boolean {
  if (item.group === 'turn engine' || item.group === 'alerts') return true;
  return item.key.startsWith('proactive.');
}

export const BehaviorParamsTab: React.FC = () => {
  const { settings, navigate, jumpToSetting, refreshLiveData, openConfirm, pushToast } =
    useConsole();

  const [statusDetail, setStatusDetail] = useState<StatusDetail | null>(null);
  const [presets, setPresets] = useState<PresetOption[]>([]);
  const [presetsAvailable, setPresetsAvailable] = useState(false);
  const [socialPaused, setSocialPaused] = useState<boolean | null>(null);
  const [loadingRuntime, setLoadingRuntime] = useState(true);
  const [runtimeError, setRuntimeError] = useState<string | null>(null);

  const [modeDraft, setModeDraft] = useState<string>('reserved2');
  const [presetDraft, setPresetDraft] = useState<string>('');
  const [savingMode, setSavingMode] = useState(false);
  const [togglingRoleMode, setTogglingRoleMode] = useState(false);
  const [togglingActivity, setTogglingActivity] = useState(false);

  const [toolSearch, setToolSearch] = useState('');
  const [toolFilter, setToolFilter] = useState<'all' | 'on' | 'off' | 'modified'>('all');
  const [showAdvanced, setShowAdvanced] = useState(false);

  const mountedRef = useRef(true);
  const loadSeqRef = useRef(0);
  const statusDetailRef = useRef<StatusDetail | null>(null);
  const modeDraftRef = useRef(modeDraft);
  const presetDraftRef = useRef(presetDraft);
  statusDetailRef.current = statusDetail;
  modeDraftRef.current = modeDraft;
  presetDraftRef.current = presetDraft;

  const loadRuntimeState = useCallback(
    async (opts?: { preserveDrafts?: boolean }) => {
      const seq = ++loadSeqRef.current;
      setLoadingRuntime(true);
      try {
        const [statusRes, presetsRes, activityRes] = await Promise.allSettled([
          fetchStatusDetail(),
          fetchPresetsOptional(),
          fetchSocialV2Activity(),
        ]);
        if (!mountedRef.current || seq !== loadSeqRef.current) return;

        const errors: string[] = [];
        if (statusRes.status === 'fulfilled') {
          const s = statusRes.value;
          const prevStatus = statusDetailRef.current;
          const prevNormalized = prevStatus ? normalizeModeValue(prevStatus.mode) : null;
          const wasModeDirty =
            prevStatus !== null &&
            (modeDraftRef.current !== prevNormalized ||
              (modeDraftRef.current === 'closed-agent' &&
                presetDraftRef.current !== (prevStatus.closedAgentPreset || '')));

          setStatusDetail(s);
          if (!opts?.preserveDrafts || !wasModeDirty) {
            setModeDraft(normalizeModeValue(s.mode));
            setPresetDraft(s.closedAgentPreset || '');
          }
          if (activityRes.status !== 'fulfilled') {
            setSocialPaused(Boolean(s.socialV2Paused));
          }
        } else {
          errors.push(`状态读取失败：${formatErrorMessage(statusRes.reason)}`);
        }

        if (presetsRes.status === 'fulfilled') {
          setPresetsAvailable(presetsRes.value.available);
          setPresets(presetsRes.value.presets);
        } else {
          errors.push(`预设列表读取失败：${formatErrorMessage(presetsRes.reason)}`);
        }

        if (activityRes.status === 'fulfilled') {
          setSocialPaused(Boolean(activityRes.value.paused));
        } else {
          errors.push(`暂停状态读取失败：${formatErrorMessage(activityRes.reason)}`);
        }

        if (errors.length > 0) {
          const msg = errors.join('；');
          setRuntimeError(msg);
          pushToast(msg, 'danger');
        } else {
          setRuntimeError(null);
        }
      } finally {
        if (mountedRef.current && seq === loadSeqRef.current) {
          setLoadingRuntime(false);
        }
      }
    },
    [pushToast],
  );

  useEffect(() => {
    mountedRef.current = true;
    void loadRuntimeState({ preserveDrafts: false });
    return () => {
      mountedRef.current = false;
    };
  }, [loadRuntimeState]);

  const modeDirty =
    statusDetail !== null &&
    (modeDraft !== normalizeModeValue(statusDetail.mode) ||
      (modeDraft === 'closed-agent' && presetDraft !== (statusDetail.closedAgentPreset || '')));

  const handleSaveMode = useCallback(async () => {
    if (savingMode || statusDetail === null) return;
    setSavingMode(true);
    try {
      const preset = modeDraft === 'closed-agent' ? presetDraft.trim() : undefined;
      const res = await saveMode(modeDraft, preset);
      if (!mountedRef.current) return;
      pushToast(
        res.dshSynced === false
          ? `运行模式已切换为 ${res.mode}（DSH 尚未就绪，稍后同步）`
          : `运行模式已切换为 ${res.mode}`,
        res.dshSynced === false ? 'warn' : 'success',
      );
      await Promise.all([loadRuntimeState({ preserveDrafts: false }), refreshLiveData()]);
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '切换运行模式失败'), 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setSavingMode(false);
      }
    }
  }, [savingMode, statusDetail, modeDraft, presetDraft, pushToast, loadRuntimeState, refreshLiveData]);

  const discardModeDraft = useCallback(() => {
    if (!statusDetailRef.current) return;
    setModeDraft(normalizeModeValue(statusDetailRef.current.mode));
    setPresetDraft(statusDetailRef.current.closedAgentPreset || '');
  }, []);

  const extraModeDirty = useMemo<ExtraDirtySource | null>(() => {
    if (!modeDirty) return null;
    return {
      count: 1,
      label: '未应用的运行模式变更',
      onSave: handleSaveMode,
      onDiscard: discardModeDraft,
    };
  }, [modeDirty, handleSaveMode, discardModeDraft]);

  const behaviorSettings = useMemo(() => {
    const allowedGroups = new Set([
      'wake',
      'reply rhythm',
      'send limits',
      'tools',
      'media',
      'voice',
      'stickers',
      'turn engine',
      'alerts',
    ]);
    return settings
      .filter((item) => allowedGroups.has(item.group))
      .map(localizeSettingItem);
  }, [settings]);

  const { drafts, setDraft, handleReset } = useSettingsDrafts(behaviorSettings, '行为参数', {
    extraDirty: extraModeDirty,
  });

  const wakeAndReplySettings = useMemo(
    () =>
      behaviorSettings.filter(
        (s) => !isAdvancedBehaviorSetting(s) && (s.group === 'wake' || s.group === 'reply rhythm'),
      ),
    [behaviorSettings],
  );

  const sendSettings = useMemo(
    () =>
      behaviorSettings.filter((s) => !isAdvancedBehaviorSetting(s) && s.group === 'send limits'),
    [behaviorSettings],
  );

  const toolSettings = useMemo(() => {
    const q = toolSearch.trim().toLowerCase();
    return behaviorSettings.filter((s) => {
      if (s.group !== 'tools') return false;
      const effectiveVal = drafts[s.key] !== undefined ? drafts[s.key] : s.value;
      if (toolFilter === 'on' && effectiveVal !== true) return false;
      if (toolFilter === 'off' && effectiveVal !== false) return false;
      if (toolFilter === 'modified' && settingValuesEqual(s.value, s.default)) {
        return false;
      }
      if (!q) return true;
      return (
        s.key.toLowerCase().includes(q) ||
        s.label.toLowerCase().includes(q) ||
        s.hint.toLowerCase().includes(q)
      );
    });
  }, [behaviorSettings, drafts, toolSearch, toolFilter]);

  const mediaVoiceStickerSettings = useMemo(
    () =>
      behaviorSettings.filter(
        (s) => s.group === 'media' || s.group === 'voice' || s.group === 'stickers',
      ),
    [behaviorSettings],
  );

  const advancedSettings = useMemo(
    () => behaviorSettings.filter((s) => isAdvancedBehaviorSetting(s)),
    [behaviorSettings],
  );

  const handleRoleModeChange = async (next: 'silent' | 'active') => {
    if (togglingRoleMode || statusDetail === null || statusDetail.roleMode === next) return;
    setTogglingRoleMode(true);
    try {
      const res = await saveRoleMode(next);
      if (!mountedRef.current) return;
      pushToast(`角色参与度已切换为 ${res.roleMode ?? next}`, 'success');
      await Promise.all([loadRuntimeState({ preserveDrafts: true }), refreshLiveData()]);
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '切换角色参与度失败'), 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setTogglingRoleMode(false);
      }
    }
  };

  const handleToggleActivity = async (nextPaused: boolean) => {
    if (togglingActivity) return;
    setTogglingActivity(true);
    try {
      const res = await setSocialV2Activity(nextPaused);
      if (!mountedRef.current) return;
      setSocialPaused(res.paused);
      pushToast(res.paused ? '已暂停社交运行时调度' : '已恢复社交运行时调度', 'success');
      await refreshLiveData();
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '更新暂停状态失败'), 'danger');
      }
    } finally {
      if (mountedRef.current) {
        setTogglingActivity(false);
      }
    }
  };

  const handleResetSocialV2 = () => {
    openConfirm({
      title: '确认重置社交状态（socialV2）？',
      description:
        '此操作将调用 POST /api/socialV2/reset：\n• 清空社交状态记录与唤醒排程缓存\n• 丢弃已入队的待处理提示词\n• 结束并注销全部活动会话及会话令牌\n• 解除当前社交暂停状态（仅选定运行时可用）\n\n确定要继续执行重置吗？',
      confirmLabel: '确认重置',
      danger: true,
      onConfirm: async () => {
        try {
          await resetSocialV2();
          if (!mountedRef.current) return;
          pushToast('已重置社交运行时状态', 'success');
          await Promise.all([loadRuntimeState({ preserveDrafts: true }), refreshLiveData()]);
        } catch (err) {
          if (mountedRef.current) {
            pushToast(formatErrorMessage(err, '重置社交状态失败'), 'danger');
          }
        }
      },
    });
  };

  const renderSettingCardItem = (item: SettingItemView) => (
    <div
      key={item.key}
      className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3"
    >
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          <Badge tone="neutral">{formatAppliesLabel(item.applies)}</Badge>
          {!settingValuesEqual(item.value, item.default) && (
            <Badge tone="accent">已自定义</Badge>
          )}
          {item.overriddenBy?.source && (
            <Badge tone="warn">覆盖: {item.overriddenBy.source}</Badge>
          )}
        </div>
        <button
          type="button"
          onClick={() => jumpToSetting(item.key)}
          className="text-[11px] font-medium text-[var(--accent)] hover:underline"
        >
          在设置库中定位
        </button>
      </div>
      <SettingField
        setting={item}
        draftValue={drafts[item.key]}
        onDraftChange={(next) => setDraft(item.key, next)}
        onReset={() => handleReset(item.key)}
      />
    </div>
  );

  const currentRoleMode = statusDetail?.roleMode === 'silent' ? 'silent' : 'active';
  const displayedModeNormalized = statusDetail ? normalizeModeValue(statusDetail.mode) : null;
  const hasKnownModeOption = MODE_OPTIONS.some((opt) => opt.value === modeDraft);

  return (
    <div className="space-y-4">
      <Card className="p-4">
        <CardHeader
          title="运行模式、参与度与活动状态"
          subtitle="实时控制运行时模式、角色是否主动发言，以及社交调度暂停开关"
          actions={
            <Button
              size="sm"
              variant="secondary"
              disabled={loadingRuntime}
              onClick={() => void loadRuntimeState({ preserveDrafts: true })}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              刷新状态
            </Button>
          }
        />

        {runtimeError && (
          <div className="mb-3 rounded-md border border-[var(--danger)]/40 bg-[var(--danger-soft)] px-3 py-2 text-xs text-[var(--text-primary)]">
            {runtimeError}
          </div>
        )}

        <div className="grid gap-4 md:grid-cols-2">
          <div className="space-y-3 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/30 p-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-xs font-semibold text-[var(--text-primary)]">运行模式</span>
              <div className="flex items-center gap-1.5">
                <Badge tone="accent">{displayedModeNormalized || '—'}</Badge>
                {statusDetail?.dshReady !== undefined && (
                  <Badge tone={statusDetail.dshReady ? 'success' : 'warn'}>
                    {statusDetail.dshReady ? 'DSH 就绪' : 'DSH 未就绪'}
                  </Badge>
                )}
              </div>
            </div>

            <div className="space-y-2">
              <select
                aria-label="选择运行模式"
                value={modeDraft}
                disabled={savingMode || statusDetail === null}
                onChange={(e) => setModeDraft(e.target.value)}
                className={INPUT_CLASS}
              >
                {!hasKnownModeOption && (
                  <option value={modeDraft}>{modeDraft}</option>
                )}
                {MODE_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>

              {modeDraft === 'closed-agent' && (
                <div>
                  <label className="mb-1 block text-[11px] text-[var(--text-muted)]">
                    closed-agent 预设
                  </label>
                  {presetsAvailable && presets.length > 0 ? (
                    <select
                      aria-label="选择 closed-agent 预设"
                      value={presetDraft}
                      disabled={savingMode || statusDetail === null}
                      onChange={(e) => setPresetDraft(e.target.value)}
                      className={INPUT_CLASS}
                    >
                      <option value="">（默认预设）</option>
                      {presets.map((p) => (
                        <option key={p.id} value={p.id}>
                          {p.id} ({p.trust})
                        </option>
                      ))}
                    </select>
                  ) : (
                    <input
                      type="text"
                      value={presetDraft}
                      disabled={savingMode || statusDetail === null}
                      onChange={(e) => setPresetDraft(e.target.value)}
                      placeholder="输入预设 ID（可选）"
                      className={INPUT_CLASS}
                    />
                  )}
                </div>
              )}

              <div className="flex items-center justify-between gap-2 pt-1">
                <span className="text-[11px] text-[var(--text-muted)]">
                  通过 POST /api/mode 立即下发。
                </span>
                <Button
                  size="sm"
                  variant="primary"
                  disabled={savingMode || statusDetail === null || !modeDirty}
                  onClick={() => void handleSaveMode()}
                >
                  {savingMode ? '应用中…' : '应用运行模式'}
                </Button>
              </div>
            </div>
          </div>

          <div className="space-y-3 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/30 p-3">
            <div className="space-y-1.5">
              <div className="flex items-center justify-between gap-2">
                <span className="text-xs font-semibold text-[var(--text-primary)]">
                  角色参与模式（roleMode）
                </span>
                <Badge tone={currentRoleMode === 'active' ? 'success' : 'warn'}>
                  {currentRoleMode === 'active' ? '主动参与' : '静默观察'}
                </Badge>
              </div>
              <SegmentedControl
                value={currentRoleMode}
                disabled={togglingRoleMode || statusDetail === null}
                onChange={(next) => void handleRoleModeChange(next)}
                options={[
                  { value: 'active', label: 'active · 主动发言' },
                  { value: 'silent', label: 'silent · 静默观察' },
                ]}
              />
            </div>

            <div className="flex items-center justify-between gap-3 border-t border-[var(--border-hairline)] pt-2.5">
              <div>
                <div className="text-xs font-semibold text-[var(--text-primary)]">
                  暂停社交调度（socialV2）
                </div>
                <div className="text-[11px] text-[var(--text-muted)]">
                  {socialPaused
                    ? '当前处于暂停状态，仅保留人工与控制面操作。'
                    : '当前正常监听并响应会话。'}
                </div>
              </div>
              <Switch
                checked={socialPaused === true}
                disabled={togglingActivity || socialPaused === null}
                onChange={(next) => void handleToggleActivity(next)}
                label="暂停社交调度"
              />
            </div>
          </div>
        </div>
      </Card>

      <Card className="p-4">
        <CardHeader
          title="规则集（即将推出 · #22）"
          subtitle="计划用于按群组/好友编排可复用规则集；当前可先在下方参数区按全局设置调优。"
          actions={<Badge tone="neutral">规划占位</Badge>}
        />
        <p className="text-xs leading-relaxed text-[var(--text-secondary)]">
          该卡位预留给即将到来的规则集功能（#22）。在此之前，对话级覆盖请在{' '}
          <button
            type="button"
            onClick={() => navigate('conversations')}
            className="font-medium text-[var(--accent)] hover:underline"
          >
            会话面板
          </button>{' '}
          调整，全局参数可直接在下方各分组保存。
        </p>
      </Card>

      <Card className="p-4">
        <CardHeader
          title="唤醒与回复节奏"
          subtitle="wait.*、wake.* 与 reply rhythm 分组设置"
          actions={<Badge tone="neutral">{wakeAndReplySettings.length} 项</Badge>}
        />
        {wakeAndReplySettings.length === 0 ? (
          <EmptyState title="暂无唤醒或回复设置" description="请确认后端设置索引已就绪。" />
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
            {wakeAndReplySettings.map(renderSettingCardItem)}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="发送与限流配额"
          subtitle="send limits 分组消息分段与速率上限设置"
          actions={<Badge tone="neutral">{sendSettings.length} 项</Badge>}
        />
        {sendSettings.length === 0 ? (
          <EmptyState title="暂无发送设置" />
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
            {sendSettings.map(renderSettingCardItem)}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="工具开关（tools.*）"
          subtitle="支持按关键字与开启状态快速筛选 30 项运行时工具能力"
          actions={
            <SegmentedControl
              value={toolFilter}
              onChange={setToolFilter}
              options={[
                { value: 'all', label: '全部' },
                { value: 'on', label: '已开启' },
                { value: 'off', label: '已关闭' },
                { value: 'modified', label: '非默认' },
              ]}
            />
          }
        />
        <div className="mb-3 max-w-sm">
          <SearchInput
            value={toolSearch}
            onChange={setToolSearch}
            placeholder="搜索 tools.* 键名或说明…"
          />
        </div>
        {toolSettings.length === 0 ? (
          <EmptyState title="没有匹配的工具开关" description="尝试清空搜索词或切换状态筛选。" />
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
            {toolSettings.map(renderSettingCardItem)}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <CardHeader
          title="媒体、语音与表情包参数"
          subtitle="media.*、voice.* 与 sticker.* 全局运行参数"
          actions={
            <Button
              size="sm"
              variant="secondary"
              onClick={() => navigate('memory', 'media')}
            >
              打开媒体与语音库
            </Button>
          }
        />
        {mediaVoiceStickerSettings.length === 0 ? (
          <EmptyState title="暂无媒体、语音或表情包设置" />
        ) : (
          <div className="grid gap-3 md:grid-cols-2">
            {mediaVoiceStickerSettings.map(renderSettingCardItem)}
          </div>
        )}
      </Card>

      <Card className="p-4">
        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <Sliders className="h-4 w-4 text-[var(--text-muted)]" />
            <div>
              <h2 className="text-[14px] font-semibold text-[var(--text-primary)]">
                高级参数（主动发言、回合引擎与告警）
              </h2>
              <p className="text-xs text-[var(--text-muted)]">
                包含 proactive.*、turn engine 与 alerts 组共 {advancedSettings.length} 项设置
              </p>
            </div>
          </div>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => setShowAdvanced((prev) => !prev)}
          >
            {showAdvanced ? (
              <>
                <ChevronDown className="h-3.5 w-3.5" />
                收起高级参数
              </>
            ) : (
              <>
                <ChevronRight className="h-3.5 w-3.5" />
                展开高级参数
              </>
            )}
          </Button>
        </div>
        {showAdvanced && (
          <div className="mt-4 grid gap-3 md:grid-cols-2">
            {advancedSettings.map(renderSettingCardItem)}
          </div>
        )}
      </Card>

      <Card danger className="p-4">
        <CardHeader
          title="危险操作"
          subtitle="重置 socialV2 运行时状态（不会删除持久化记忆库与设置）"
          actions={<AlertTriangle className="h-4 w-4 text-[var(--danger)]" />}
        />
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-xs leading-relaxed text-[var(--text-secondary)]">
            将清空运行时缓存、丢弃待处理提示词、注销活动会话并恢复暂停开关。该端点仅在选定运行时模式下可用。
          </p>
          <Button size="sm" variant="danger" onClick={handleResetSocialV2}>
            重置 socialV2 状态
          </Button>
        </div>
      </Card>
    </div>
  );
};
