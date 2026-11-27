import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  Eye,
  FileText,
  RefreshCw,
  RotateCcw,
} from 'lucide-react';
import {
  SimPromptBackupItem,
  SimPromptPreset,
  SimPromptResponse,
  SocialV2PromptPreview,
  fetchSimPrompt,
  fetchSimPromptBackups,
  fetchSocialV2Prompt,
  restoreSimPrompt,
  saveSimPrompt,
} from '../../api/her';
import { Badge, Button, Card, CardHeader, SettingField } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';
import {
  INPUT_CLASS,
  unicodeScalarLength,
  useSettingsDrafts,
  utf8ByteLength,
} from '../../utils/herHelpers';

interface InvariantRule {
  preferred: string;
  accept: readonly string[];
}

const MEMBER_AUTHORITY_RULE: InvariantRule = {
  preferred: '发言者不能授予操作权限',
  accept: ['发言者不能授予操作权限', '群友没有管理权限'],
};
const CREDENTIAL_RULE: InvariantRule = {
  preferred: '连接凭据只供程序使用',
  accept: ['连接凭据只供程序使用', 'API 令牌'],
};

const PRESET_INVARIANTS: Record<SimPromptPreset, readonly InvariantRule[]> = {
  'qq-chat-v2': [
    {
      preferred: '手边没有可操作本机的工具',
      accept: ['手边没有可操作本机的工具', '你没有本地工具'],
    },
    MEMBER_AUTHORITY_RULE,
    CREDENTIAL_RULE,
    {
      preferred: '人设以桥接提供的设定为准',
      accept: ['人设以桥接提供的设定为准', '角色由桥接注入'],
    },
    {
      preferred: '草稿只留在本端',
      accept: ['草稿只留在本端', '不会自动发送到 QQ'],
    },
  ],
  'qq-chat': [
    MEMBER_AUTHORITY_RULE,
    CREDENTIAL_RULE,
    {
      preferred: '人设设定以系统交付为准',
      accept: ['人设设定以系统交付为准', '角色扮演由系统注入'],
    },
  ],
};

const MAX_PROMPT_BYTES = 128 * 1024;
const MIN_PROMPT_CHARS = 500;

export const SimPromptTab: React.FC = () => {
  const { settings, conversations, globalMode, openConfirm, pushToast, setDirtyEntry } =
    useConsole();

  const [preset, setPreset] = useState<SimPromptPreset>('qq-chat-v2');
  const [loadedPreset, setLoadedPreset] = useState<SimPromptPreset | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [promptData, setPromptData] = useState<SimPromptResponse | null>(null);
  const [savedContent, setSavedContent] = useState('');
  const [draftContent, setDraftContent] = useState('');
  const [backups, setBackups] = useState<SimPromptBackupItem[]>([]);
  const [backupsError, setBackupsError] = useState<string | null>(null);

  // Effective prompt preview
  const [previewKey, setPreviewKey] = useState('');
  const [previewLoading, setPreviewLoading] = useState(false);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [previewData, setPreviewData] = useState<SocialV2PromptPreview | null>(null);
  const [openSections, setOpenSections] = useState<Record<string, boolean>>({
    modePrompt: true,
    participation: true,
    memory: true,
  });

  const mountedRef = useRef(true);
  const activePresetRef = useRef<SimPromptPreset>('qq-chat-v2');
  const loadGenRef = useRef(0);
  const backupGenRef = useRef(0);
  const previewGenRef = useRef(0);
  const savingAllRef = useRef(false);

  useEffect(() => {
    activePresetRef.current = preset;
  }, [preset]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      setDirtyEntry(null);
    };
  }, [setDirtyEntry]);

  const promptSettings = useMemo(
    () => settings.filter((s) => s.group === 'prompt text' || s.key.startsWith('prompt.')),
    [settings],
  );

  const {
    drafts: settingDrafts,
    dirtyKeys: dirtySettingKeys,
    setDraft: setSettingDraft,
    discardAll: discardSettings,
    saveAll: saveSettings,
    handleReset: handleResetPromptSetting,
  } = useSettingsDrafts(promptSettings, '模式/行为提示词', { manageDirtyBar: false });

  const promptAligned = loadedPreset === preset && promptData !== null;
  const isPromptDirty = promptAligned && draftContent !== savedContent;

  const charCount = useMemo(() => unicodeScalarLength(draftContent.trim()), [draftContent]);
  const byteCount = useMemo(
    () => utf8ByteLength(draftContent.replace(/\r\n/g, '\n').replace(/\n+$/, '')),
    [draftContent],
  );

  const missingInvariants = useMemo(
    () =>
      PRESET_INVARIANTS[preset]
        .filter((rule) => !rule.accept.some((token) => draftContent.includes(token)))
        .map((rule) => rule.preferred),
    [preset, draftContent],
  );

  const loadBackups = useCallback(async (targetPreset: SimPromptPreset) => {
    const gen = ++backupGenRef.current;
    setBackupsError(null);
    try {
      const bakRes = await fetchSimPromptBackups(targetPreset);
      if (!mountedRef.current || gen !== backupGenRef.current || activePresetRef.current !== targetPreset) {
        return;
      }
      setBackups(bakRes.backups ?? []);
    } catch (err) {
      if (!mountedRef.current || gen !== backupGenRef.current || activePresetRef.current !== targetPreset) {
        return;
      }
      setBackups([]);
      setBackupsError(formatErrorMessage(err, '读取提示词备份列表失败'));
    }
  }, []);

  const loadPreset = useCallback(
    async (targetPreset: SimPromptPreset) => {
      const gen = ++loadGenRef.current;
      backupGenRef.current += 1;
      setLoading(true);
      setError(null);
      setLoadedPreset(null);
      setPromptData(null);
      setSavedContent('');
      setDraftContent('');
      setBackups([]);
      setBackupsError(null);
      try {
        const res = await fetchSimPrompt(targetPreset);
        if (!mountedRef.current || gen !== loadGenRef.current || activePresetRef.current !== targetPreset) {
          return;
        }
        setPromptData(res);
        setLoadedPreset(targetPreset);
        setSavedContent(res.content);
        setDraftContent(res.content);
      } catch (err) {
        if (!mountedRef.current || gen !== loadGenRef.current || activePresetRef.current !== targetPreset) {
          return;
        }
        setError(formatErrorMessage(err, '加载仿真提示词失败'));
      } finally {
        if (mountedRef.current && gen === loadGenRef.current) {
          setLoading(false);
        }
      }
      void loadBackups(targetPreset);
    },
    [loadBackups],
  );

  useEffect(() => {
    void loadPreset(preset);
  }, [preset, loadPreset]);

  useEffect(() => {
    if (!previewKey && conversations.length > 0) {
      setPreviewKey(conversations[0].key);
    }
  }, [previewKey, conversations]);

  const loadPreview = useCallback(async (key: string) => {
    const trimmed = key.trim();
    if (!trimmed) return;
    const gen = ++previewGenRef.current;
    setPreviewLoading(true);
    setPreviewError(null);
    try {
      const res = await fetchSocialV2Prompt(trimmed);
      if (!mountedRef.current || gen !== previewGenRef.current) return;
      setPreviewData(res);
    } catch (err) {
      if (!mountedRef.current || gen !== previewGenRef.current) return;
      setPreviewData(null);
      setPreviewError(formatErrorMessage(err, '获取生效提示词预览失败'));
    } finally {
      if (mountedRef.current && gen === previewGenRef.current) {
        setPreviewLoading(false);
      }
    }
  }, []);

  const resolvedModePrompt = useMemo(() => {
    if (!previewData) return null;
    const ownerItem = settings.find((s) => s.key === 'ownerQQ');
    const ownerRaw = ownerItem?.overriddenBy?.value ?? ownerItem?.value;
    const ownerStr =
      typeof ownerRaw === 'number' || typeof ownerRaw === 'string' ? String(ownerRaw).trim() : '';
    const isOwnerPrivate = Boolean(ownerStr && previewData.key === `private:${ownerStr}`);

    let settingKey: string | null = null;
    if (globalMode === 'chat' || globalMode === 'reserved') {
      settingKey = isOwnerPrivate ? 'prompt.mode.ownerPrivate' : 'prompt.mode.chat';
    } else if (globalMode === 'reserved2' || globalMode === 'simulation') {
      settingKey = isOwnerPrivate ? 'prompt.mode.reserved2Owner' : 'prompt.mode.reserved2';
    } else if (globalMode === 'closed-agent') {
      settingKey = isOwnerPrivate ? 'prompt.mode.closedAgent' : null;
    }
    if (!settingKey) {
      return {
        settingKey: null,
        isDraft: false,
        text:
          globalMode === 'closed-agent'
            ? `管理员代理模式 (${globalMode}) 仅向管理员私聊注入 prompt.mode.closedAgent，该会话不注入`
            : `当前模式 (${globalMode}) 不注入自有引擎 prompt.mode.* 文本`,
      };
    }
    const item = settings.find((s) => s.key === settingKey);
    const val = settingKey in settingDrafts ? settingDrafts[settingKey] : item?.value;
    const text = typeof val === 'string' && val.trim() ? val : '（未配置自定义模式提示词）';
    return {
      settingKey,
      isDraft: settingKey in settingDrafts,
      text,
    };
  }, [previewData, settings, globalMode, settingDrafts]);

  const handleSaveAll = useCallback(async () => {
    if (savingAllRef.current) return;
    if (isPromptDirty) {
      if (!promptAligned) {
        pushToast('当前预设尚未加载完成，请稍后再保存', 'warn');
        return;
      }
      if (charCount < MIN_PROMPT_CHARS) {
        pushToast(`仿真提示词去除首尾空白后至少需要 ${MIN_PROMPT_CHARS} 个字符`, 'warn');
        return;
      }
      if (byteCount > MAX_PROMPT_BYTES) {
        pushToast('仿真提示词 UTF-8 体积不能超过 128 KiB', 'warn');
        return;
      }
      if (missingInvariants.length > 0) {
        pushToast(`缺少必需安全语句：${missingInvariants.join('、')}`, 'warn');
        return;
      }
    }

    savingAllRef.current = true;
    setBusy(true);
    try {
      if (isPromptDirty) {
        const targetPreset = preset;
        const submittedContent = draftContent;
        const res = await saveSimPrompt(targetPreset, submittedContent);
        if (!mountedRef.current) return;
        if (activePresetRef.current === targetPreset) {
          setSavedContent(submittedContent);
        }
        pushToast(
          res.unchanged
            ? '仿真提示词与服务端一致'
            : res.requiresRestart
              ? '仿真提示词已保存并生成备份（需重启相关后端生效）'
              : '仿真提示词已保存',
          'success',
        );
        if (activePresetRef.current === targetPreset) {
          await loadPreset(targetPreset);
        }
      }
      if (dirtySettingKeys.length > 0) {
        await saveSettings();
      }
    } catch (err) {
      if (mountedRef.current) {
        pushToast(formatErrorMessage(err, '保存仿真提示词失败'), 'danger');
      }
    } finally {
      savingAllRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  }, [
    isPromptDirty,
    promptAligned,
    charCount,
    byteCount,
    missingInvariants,
    preset,
    draftContent,
    loadPreset,
    dirtySettingKeys.length,
    saveSettings,
    pushToast,
  ]);

  const handleDiscardAll = useCallback(() => {
    setDraftContent(savedContent);
    discardSettings();
  }, [savedContent, discardSettings]);

  const totalDirtyCount = (isPromptDirty ? 1 : 0) + dirtySettingKeys.length;

  useEffect(() => {
    if (totalDirtyCount > 0) {
      setDirtyEntry({
        count: totalDirtyCount,
        label: `仿真提示词与规则文本有 ${totalDirtyCount} 项未保存更改`,
        onSave: handleSaveAll,
        onDiscard: handleDiscardAll,
      });
    } else {
      setDirtyEntry(null);
    }
  }, [totalDirtyCount, setDirtyEntry, handleSaveAll, handleDiscardAll]);

  const handleSwitchPreset = (nextPreset: SimPromptPreset) => {
    if (nextPreset === preset) return;
    if (isPromptDirty) {
      openConfirm({
        title: '切换预设将丢失当前未保存修改？',
        description: '当前预设编辑器有尚未保存的改动，切换预设会丢弃这些改动。',
        confirmLabel: '放弃并切换',
        danger: true,
        onConfirm: () => {
          activePresetRef.current = nextPreset;
          setPreset(nextPreset);
        },
      });
      return;
    }
    activePresetRef.current = nextPreset;
    setPreset(nextPreset);
  };

  const handleRefreshPreset = () => {
    if (isPromptDirty) {
      openConfirm({
        title: '放弃未保存修改并重新加载预设？',
        description: '刷新将从服务端重新读取预设正文并覆盖当前未保存编辑。',
        confirmLabel: '刷新并覆盖',
        danger: true,
        onConfirm: () => void loadPreset(preset),
      });
      return;
    }
    void loadPreset(preset);
  };

  const handleRestoreBackup = (file: string) => {
    if (!promptAligned) return;
    const targetPreset = preset;
    openConfirm({
      title: `确认从备份还原「${targetPreset}」？`,
      description: isPromptDirty
        ? `将先备份当前文件再从 ${file} 还原，并通过预设不变量校验；同时会丢弃当前编辑器中尚未保存的修改。`
        : `将先备份当前文件再从 ${file} 还原，并通过预设不变量校验。`,
      confirmLabel: '确认还原',
      danger: true,
      onConfirm: async () => {
        setBusy(true);
        try {
          const res = await restoreSimPrompt(targetPreset, file);
          if (!mountedRef.current) return;
          pushToast(`已从 ${res.restoredFrom} 还原提示词`, 'success');
          if (activePresetRef.current === targetPreset) {
            await loadPreset(targetPreset);
          }
        } catch (err) {
          pushToast(formatErrorMessage(err, '还原备份失败'), 'danger');
        } finally {
          if (mountedRef.current) setBusy(false);
        }
      },
    });
  };

  const toggleSection = (sec: string) => {
    setOpenSections((prev) => ({ ...prev, [sec]: !prev[sec] }));
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader
          title={
            <span className="flex items-center gap-2">
              <FileText className="h-4 w-4 text-[var(--accent)]" />
              <span>DSH 仿真预设提示词</span>
              {promptAligned && promptData?.status && (
                <Badge tone={promptData.status.installed ? 'success' : 'warn'}>
                  {promptData.status.installed ? '已安装' : '未安装'}
                </Badge>
              )}
            </span>
          }
          subtitle="在此修改并回写仿真配置档案内的核心提示正文；落盘前将自动核验文本篇幅与安全边界。"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <select
                value={preset}
                aria-label="选择仿真预设"
                onChange={(e) => handleSwitchPreset(e.target.value as SimPromptPreset)}
                className="h-7 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)]"
              >
                <option value="qq-chat-v2">二代仿真 (qq-chat-v2)</option>
                <option value="qq-chat">一代仿真 (qq-chat)</option>
              </select>
              <Button size="sm" variant="ghost" disabled={loading} onClick={handleRefreshPreset}>
                <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
                刷新
              </Button>
            </div>
          }
        />

        <div className="space-y-3 p-4 text-xs">
          {error && (
            <div className="flex items-center justify-between gap-2 rounded-md border border-rose-300 bg-[var(--danger-soft)] p-3 text-[var(--danger)]">
              <span>{error}</span>
              <Button size="sm" variant="secondary" onClick={() => void loadPreset(preset)}>
                重试
              </Button>
            </div>
          )}

          {loading && !promptAligned && (
            <div className="text-[var(--text-muted)]">正在加载 {preset} 预设提示词…</div>
          )}

          {promptAligned && promptData && (
            <>
              <div className="flex flex-wrap items-center justify-between gap-2 text-[11px] text-[var(--text-muted)]">
                <span className="font-mono">{promptData.status.sourceFile}</span>
                <span className="tabular-nums">
                  有效字符 {charCount}（要求 ≥ {MIN_PROMPT_CHARS}） · {(byteCount / 1024).toFixed(1)} / 128 KB
                </span>
              </div>

              {promptData.warnings.length > 0 && (
                <div className="rounded-md border border-amber-300 bg-[var(--warn-soft)] p-2.5 text-[11px] text-[var(--warn)] dark:border-amber-900">
                  {promptData.warnings.join('；')}
                </div>
              )}

              <div className="flex flex-wrap items-center gap-1.5 text-[11px]">
                <span className="text-[var(--text-muted)]">必需安全语句：</span>
                {PRESET_INVARIANTS[preset].map((rule) => {
                  const ok = rule.accept.some((token) => draftContent.includes(token));
                  return (
                    <Badge key={rule.preferred} tone={ok ? 'success' : 'danger'}>
                      {ok ? '✓' : '✕'} {rule.preferred}
                    </Badge>
                  );
                })}
              </div>

              <textarea
                rows={14}
                value={draftContent}
                onChange={(e) => setDraftContent(e.target.value)}
                disabled={busy || !promptAligned}
                aria-label="仿真预设提示词正文"
                className={`${INPUT_CLASS} font-mono leading-relaxed`}
              />

              {missingInvariants.length > 0 && (
                <div className="flex items-center gap-1.5 text-[11px] text-[var(--danger)]">
                  <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
                  <span>保存前请保留以上缺失的安全不变量短语。</span>
                </div>
              )}
            </>
          )}
        </div>
      </Card>

      {promptAligned && (
        <Card danger>
          <CardHeader
            title={
              <span className="flex items-center gap-2 text-[var(--danger)]">
                <AlertTriangle className="h-4 w-4" />
                <span>危险操作：从备份还原仿真提示词（最多保留 10 份）</span>
              </span>
            }
            subtitle="还原会先备份当前磁盘文件并用所选备份覆盖预设正文，同时丢弃尚未保存的编辑草稿。"
          />
          <div className="space-y-2 p-4 text-xs">
            {backupsError ? (
              <div className="flex items-center justify-between gap-2 rounded border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[11px] text-[var(--danger)]">
                <span>{backupsError}</span>
                <Button size="sm" variant="secondary" onClick={() => void loadBackups(preset)}>
                  重试
                </Button>
              </div>
            ) : backups.length === 0 ? (
              <div className="text-[11px] text-[var(--text-muted)]">暂无提示词备份</div>
            ) : (
              <div className="grid grid-cols-1 gap-1.5 sm:grid-cols-2">
                {backups.map((bak) => (
                  <div
                    key={bak.file}
                    className="flex items-center justify-between gap-2 rounded border border-rose-200 bg-[var(--bg-surface)] px-2.5 py-1.5 dark:border-rose-900/60"
                  >
                    <div className="min-w-0">
                      <div className="truncate font-mono text-[11px] text-[var(--text-primary)]">
                        {bak.file}
                      </div>
                      <div className="text-[10.5px] text-[var(--text-muted)]">
                        {formatDateTimeMs(bak.mtime)}
                      </div>
                    </div>
                    <Button
                      size="sm"
                      variant="danger"
                      disabled={busy || !promptAligned}
                      onClick={() => handleRestoreBackup(bak.file)}
                    >
                      <RotateCcw className="h-3 w-3" />
                      还原
                    </Button>
                  </div>
                ))}
              </div>
            )}
          </div>
        </Card>
      )}

      <Card>
        <CardHeader
          title="模式提示词与行为规则文本 (/api/settings)"
          subtitle="HER 自有引擎及简报公共行为规则文本；修改后将在底部栏统一保存。"
        />
        <div className="grid grid-cols-1 gap-4 p-4 lg:grid-cols-2">
          {promptSettings.map((setting) => {
            const currentDraft =
              setting.key in settingDrafts ? settingDrafts[setting.key] : setting.value;
            return (
              <div
                key={setting.key}
                className="rounded-md border border-[var(--border-hairline)] p-3"
              >
                <SettingField
                  setting={setting}
                  draftValue={currentDraft}
                  onDraftChange={(val) => setSettingDraft(setting.key, val)}
                  onReset={() => handleResetPromptSetting(setting.key)}
                />
              </div>
            );
          })}
        </div>
      </Card>

      <Card>
        <CardHeader
          title={
            <span className="flex items-center gap-2">
              <Eye className="h-4 w-4 text-[var(--accent)]" />
              <span>当前生效提示词与上下文预览 (/api/socialV2/prompt)</span>
            </span>
          }
          subtitle="查看指定会话当前生效的 Mode prompt、参与规则、记忆摘要、黑话与表情包策略。"
          actions={
            <div className="flex items-center gap-2">
              <select
                value={previewKey}
                aria-label="选择提示词预览会话"
                onChange={(e) => setPreviewKey(e.target.value)}
                className="h-7 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs text-[var(--text-primary)]"
              >
                {conversations.length === 0 && <option value="">暂无会话</option>}
                {conversations.map((c) => (
                  <option key={c.key} value={c.key}>
                    {c.name ? `${c.name} (${c.key})` : c.key}
                  </option>
                ))}
              </select>
              <Button
                size="sm"
                variant="secondary"
                disabled={previewLoading || !previewKey}
                onClick={() => void loadPreview(previewKey)}
              >
                <RefreshCw className={`h-3.5 w-3.5 ${previewLoading ? 'animate-spin' : ''}`} />
                加载预览
              </Button>
            </div>
          }
        />
        <div className="space-y-2.5 p-4 text-xs">
          {previewError && (
            <div className="rounded border border-rose-300 bg-[var(--danger-soft)] p-2.5 text-[var(--danger)]">
              {previewError}
            </div>
          )}
          {!previewData ? (
            <div className="text-[var(--text-muted)]">
              选择会话后点击「加载预览」以查看当前运行时拼接结果。
            </div>
          ) : (
            <div className="space-y-2">
              <div className="flex flex-wrap items-center gap-2 text-[11px] text-[var(--text-secondary)]">
                <Badge tone="accent">{previewData.key}</Badge>
                <Badge tone="neutral">模式 {globalMode}</Badge>
                <span>时间：{previewData.time}</span>
                <span>角色：{previewData.role?.name || '（默认）'}</span>
                <span>未读 {previewData.unreadCount} · 近期 {previewData.recentCount}</span>
                <span>已启用工具 {previewData.enabledTools.length} 个</span>
              </div>

              {(
                [
                  {
                    id: 'modePrompt',
                    title: resolvedModePrompt?.settingKey
                      ? `当前模式提示词 (${resolvedModePrompt.settingKey}${resolvedModePrompt.isDraft ? ' · 草稿未保存' : ''})`
                      : '当前模式提示词 (prompt.mode.*)',
                    body: resolvedModePrompt?.text ?? '（无）',
                  },
                  { id: 'participation', title: '参与规则与会话上下文', body: previewData.participation },
                  { id: 'memory', title: '注入记忆块', body: previewData.memory || '（空）' },
                  {
                    id: 'slang',
                    title: `黑话词库提示 (${previewData.slang.enabled ? '启用' : '关闭'})`,
                    body: previewData.slang.block || '（空）',
                  },
                  {
                    id: 'stickers',
                    title: `表情包策略 (${previewData.stickers.total} 个)`,
                    body: `${previewData.stickers.strategy}\n\n${previewData.stickers.context}`.trim(),
                  },
                  {
                    id: 'tools',
                    title: '已启用工具与回复节奏',
                    body: JSON.stringify(
                      {
                        enabledTools: previewData.enabledTools,
                        replyTiming: previewData.replyTiming,
                        voices: previewData.voices,
                      },
                      null,
                      2,
                    ),
                  },
                ] as const
              ).map((sec) => {
                const isOpen = Boolean(openSections[sec.id]);
                return (
                  <div
                    key={sec.id}
                    className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/30"
                  >
                    <button
                      type="button"
                      aria-expanded={isOpen}
                      onClick={() => toggleSection(sec.id)}
                      className="flex w-full items-center justify-between px-3 py-2 text-left font-medium text-[var(--text-primary)]"
                    >
                      <span>{sec.title}</span>
                      {isOpen ? (
                        <ChevronDown className="h-3.5 w-3.5 text-[var(--text-muted)]" />
                      ) : (
                        <ChevronRight className="h-3.5 w-3.5 text-[var(--text-muted)]" />
                      )}
                    </button>
                    {isOpen && (
                      <pre className="max-h-72 overflow-auto border-t border-[var(--border-hairline)] p-3 font-mono text-[11px] whitespace-pre-wrap text-[var(--text-secondary)]">
                        {sec.body}
                      </pre>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </Card>
    </div>
  );
};
