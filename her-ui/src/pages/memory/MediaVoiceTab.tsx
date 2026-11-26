import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Image as ImageIcon, Plus, RefreshCw, Send, Volume2, X } from 'lucide-react';
import {
  StickerImageResponse,
  StickerView,
  VoiceListResponse,
  VoiceSendResponse,
  collectSticker,
  fetchStickerImage,
  fetchStickers,
  fetchVoices,
  saveStickerNote,
  saveStickerRemark,
  sendVoice,
  syncStickers,
} from '../../api/her';
import { EmptyState, SearchInput } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, CardHeader, SettingField } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../../utils/format';
import {
  INPUT_CLASS,
  unicodeScalarLength,
  useSettingsDrafts,
  utf16Length,
} from '../../utils/herHelpers';

function sameStringArray(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, idx) => v === b[idx]);
}

export const MediaVoiceTab: React.FC = () => {
  const { settings, conversations, globalMode, pushToast } = useConsole();

  // Stickers state
  const [stickerQuery, setStickerQuery] = useState('');
  const [stickers, setStickers] = useState<StickerView[]>([]);
  const [stickerTotal, setStickerTotal] = useState(0);
  const [stickerSyncedAt, setStickerSyncedAt] = useState(0);
  const [stickersLoading, setStickersLoading] = useState(true);
  const [stickersError, setStickersError] = useState<string | null>(null);
  const [stickerBusy, setStickerBusy] = useState(false);

  const [previewKey, setPreviewKey] = useState('');
  const [previewSticker, setPreviewSticker] = useState<StickerImageResponse | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);

  // Editing sticker preserving tags as string[]
  const [editingSticker, setEditingSticker] = useState<{
    id: string;
    note: string;
    usage: string;
    tags: string[];
    tagDraft: string;
    remark: string;
    originalRemark: string;
  } | null>(null);

  const [collectOpen, setCollectOpen] = useState(false);
  const [collectKey, setCollectKey] = useState('');
  const [collectMessageId, setCollectMessageId] = useState('');
  const [collectSeq, setCollectSeq] = useState('');
  const [collectRemark, setCollectRemark] = useState('');

  // Voice state
  const [voiceData, setVoiceData] = useState<VoiceListResponse | null>(null);
  const [voicesLoading, setVoicesLoading] = useState(true);
  const [voicesError, setVoicesError] = useState<string | null>(null);
  const [voiceBusy, setVoiceBusy] = useState(false);

  const [voiceTarget, setVoiceTarget] = useState('');
  const [voiceCustomTarget, setVoiceCustomTarget] = useState('');
  const [voiceFile, setVoiceFile] = useState('');
  const [voiceVolume, setVoiceVolume] = useState('1');
  const [voiceLoudness, setVoiceLoudness] = useState<'quiet' | 'normal' | 'loud'>('normal');
  const [voiceNormalize, setVoiceNormalize] = useState(false);
  const [voiceVerify, setVoiceVerify] = useState(true);
  const [voiceReadback, setVoiceReadback] = useState(false);
  const [voiceResult, setVoiceResult] = useState<VoiceSendResponse | null>(null);

  const mountedRef = useRef(true);
  const stickerGenRef = useRef(0);
  const voiceGenRef = useRef(0);
  const previewGenRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // Active-mode allowlisted destinations (voice.rs / console_agent.rs contract)
  const allowlistedDestinations = useMemo(() => {
    const keys = new Set<string>();
    const addList = (val: unknown, prefix: string) => {
      if (!Array.isArray(val)) return;
      for (const item of val) {
        const s = String(item).trim();
        if (!s) continue;
        const normalized = s.includes(':') ? s : `${prefix}${s}`;
        if (/^(group|private):[1-9]\d*$/.test(normalized)) {
          keys.add(normalized);
        }
      }
    };
    if (globalMode === 'closed-agent') {
      const ownerItem = settings.find((s) => s.key === 'ownerQQ');
      const ownerRaw = ownerItem?.overriddenBy?.value ?? ownerItem?.value;
      const ownerStr =
        typeof ownerRaw === 'number' || typeof ownerRaw === 'string' ? String(ownerRaw).trim() : '';
      if (/^[1-9]\d*$/.test(ownerStr)) {
        keys.add(`private:${ownerStr}`);
      }
    } else {
      addList(settings.find((s) => s.key === 'allow.groups')?.value, 'group:');
      addList(settings.find((s) => s.key === 'allow.private')?.value, 'private:');
    }
    return Array.from(keys);
  }, [settings, globalMode]);

  useEffect(() => {
    if (allowlistedDestinations.length > 0) {
      const first = allowlistedDestinations[0];
      if (!previewKey || !allowlistedDestinations.includes(previewKey)) setPreviewKey(first);
      if (!collectKey || !allowlistedDestinations.includes(collectKey)) setCollectKey(first);
      if (!voiceTarget || (voiceTarget !== '__custom__' && !allowlistedDestinations.includes(voiceTarget))) {
        setVoiceTarget(first);
      }
    } else if (!voiceTarget) {
      setVoiceTarget('__custom__');
    }
  }, [allowlistedDestinations, previewKey, collectKey, voiceTarget]);

  const mediaSettings = useMemo(
    () =>
      settings.filter(
        (s) =>
          s.group === 'stickers' ||
          s.group === 'voice' ||
          s.key === 'socialV2.tools.sendVoice',
      ),
    [settings],
  );

  const collectRemarkMaxChars = useMemo(() => {
    const val = settings.find((s) => s.key === 'socialV2.sticker.collect.maxRemarkChars')?.value;
    return typeof val === 'number' && val > 0 ? Math.round(val) : 20;
  }, [settings]);

  const {
    drafts: settingDrafts,
    setDraft: setSettingDraft,
    handleReset: handleResetMediaSetting,
  } = useSettingsDrafts(mediaSettings, '表情包与语音参数');

  const loadStickers = useCallback(async (q: string) => {
    const gen = ++stickerGenRef.current;
    setStickersLoading(true);
    setStickersError(null);
    try {
      const res = await fetchStickers(q, 60);
      if (!mountedRef.current || gen !== stickerGenRef.current) return;
      setStickers(res.stickers ?? []);
      setStickerTotal(res.total ?? 0);
      setStickerSyncedAt(res.syncedAt ?? 0);
    } catch (err) {
      if (!mountedRef.current || gen !== stickerGenRef.current) return;
      setStickersError(formatErrorMessage(err, '加载表情包列表失败'));
    } finally {
      if (mountedRef.current && gen === stickerGenRef.current) {
        setStickersLoading(false);
      }
    }
  }, []);

  const loadVoices = useCallback(async (probe = true) => {
    const gen = ++voiceGenRef.current;
    setVoicesLoading(true);
    setVoicesError(null);
    try {
      const res = await fetchVoices(probe);
      if (!mountedRef.current || gen !== voiceGenRef.current) return;
      setVoiceData(res);
      setVoiceFile((prev) => prev || res.voices?.[0]?.name || '');
    } catch (err) {
      if (!mountedRef.current || gen !== voiceGenRef.current) return;
      setVoicesError(formatErrorMessage(err, '探测语音库失败'));
    } finally {
      if (mountedRef.current && gen === voiceGenRef.current) {
        setVoicesLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    void loadStickers('');
    void loadVoices(true);
  }, [loadStickers, loadVoices]);

  const handleSyncStickers = async () => {
    setStickerBusy(true);
    try {
      const res = await syncStickers();
      if (!mountedRef.current) return;
      pushToast(`已同步 ${res.total} 个 QQ 表情包`, 'success');
      await loadStickers(stickerQuery);
    } catch (err) {
      pushToast(formatErrorMessage(err, '同步 QQ 表情包失败'), 'danger');
    } finally {
      if (mountedRef.current) setStickerBusy(false);
    }
  };

  const handlePreviewSticker = async (stickerId: string) => {
    if (!previewKey) {
      pushToast('请先选择用于图像拉取的会话', 'warn');
      return;
    }
    const gen = ++previewGenRef.current;
    setPreviewLoading(true);
    try {
      const res = await fetchStickerImage(previewKey, stickerId);
      if (!mountedRef.current || gen !== previewGenRef.current) return;
      setPreviewSticker(res);
    } catch (err) {
      if (!mountedRef.current || gen !== previewGenRef.current) return;
      pushToast(formatErrorMessage(err, '获取表情包预览图失败'), 'danger');
    } finally {
      if (mountedRef.current && gen === previewGenRef.current) {
        setPreviewLoading(false);
      }
    }
  };

  const handleAddTag = () => {
    if (!editingSticker) return;
    const candidate = editingSticker.tagDraft.trim();
    if (!candidate) return;
    if (unicodeScalarLength(candidate) > 24) {
      pushToast('单个标签不能超过 24 个字符', 'warn');
      return;
    }
    if (editingSticker.tags.length >= 20) {
      pushToast('标签最多 20 个', 'warn');
      return;
    }
    if (!editingSticker.tags.includes(candidate)) {
      setEditingSticker((prev) =>
        prev ? { ...prev, tags: [...prev.tags, candidate], tagDraft: '' } : prev,
      );
    } else {
      setEditingSticker((prev) => (prev ? { ...prev, tagDraft: '' } : prev));
    }
  };

  const handleSaveStickerEdit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!editingSticker) return;
    const targetId = editingSticker.id;
    const submittedNote = editingSticker.note;
    const submittedUsage = editingSticker.usage;
    const submittedTags = [...editingSticker.tags];

    if (utf16Length(submittedNote) > 200 || utf16Length(submittedUsage) > 200) {
      pushToast('本地备注与适用场景不能超过 200 个字符', 'warn');
      return;
    }
    if (submittedTags.length > 20 || submittedTags.some((t) => unicodeScalarLength(t) > 24)) {
      pushToast('标签最多 20 个，且每个标签不超过 24 个字符', 'warn');
      return;
    }
    setStickerBusy(true);
    try {
      await saveStickerNote({
        stickerId: targetId,
        note: submittedNote,
        usage: submittedUsage,
        tags: submittedTags,
      });
      if (!mountedRef.current) return;
      let remarkPending = false;
      setEditingSticker((prev) => {
        if (!prev || prev.id !== targetId) return prev;
        const hasUnsavedRemark = prev.remark.trim() !== prev.originalRemark.trim();
        remarkPending = hasUnsavedRemark;
        const stillMatchesSubmitted =
          prev.note === submittedNote &&
          prev.usage === submittedUsage &&
          sameStringArray(prev.tags, submittedTags);
        return hasUnsavedRemark || !stillMatchesSubmitted ? prev : null;
      });
      pushToast(
        remarkPending ? '已保存本地备注（右侧 QQ 远端备注尚未提交）' : '已保存本地表情备注与标签',
        'success',
      );
      await loadStickers(stickerQuery);
    } catch (err) {
      pushToast(formatErrorMessage(err, '保存本地备注失败'), 'danger');
    } finally {
      if (mountedRef.current) setStickerBusy(false);
    }
  };

  const handleSaveQqRemark = async () => {
    if (!editingSticker) return;
    const targetId = editingSticker.id;
    const submittedRemark = editingSticker.remark.trim();
    if (unicodeScalarLength(submittedRemark) > 50) {
      pushToast('QQ 远端表情备注不能超过 50 个字符', 'warn');
      return;
    }
    setStickerBusy(true);
    try {
      await saveStickerRemark(targetId, submittedRemark);
      if (!mountedRef.current) return;
      pushToast('已更新 QQ 远端表情备注', 'success');
      setEditingSticker((prev) => {
        if (!prev || prev.id !== targetId) return prev;
        return {
          ...prev,
          remark: prev.remark.trim() === submittedRemark ? submittedRemark : prev.remark,
          originalRemark: submittedRemark,
        };
      });
      await loadStickers(stickerQuery);
    } catch (err) {
      pushToast(formatErrorMessage(err, '更新 QQ 备注失败'), 'danger');
    } finally {
      if (mountedRef.current) setStickerBusy(false);
    }
  };

  const handleCollect = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!collectKey || (!collectMessageId.trim() && !collectSeq.trim())) {
      pushToast('请填写会话及 messageId 或消息 seq', 'warn');
      return;
    }
    if (unicodeScalarLength(collectRemark.trim()) > collectRemarkMaxChars) {
      pushToast(`收藏备注不能超过 ${collectRemarkMaxChars} 个字符`, 'warn');
      return;
    }
    setStickerBusy(true);
    try {
      const res = await collectSticker({
        key: collectKey,
        messageId: collectMessageId.trim() || undefined,
        seq: collectSeq.trim() || undefined,
        remark: collectRemark.trim() || undefined,
      });
      if (!mountedRef.current) return;
      pushToast(`已收藏表情 ${res.emojiId}`, 'success');
      setCollectMessageId('');
      setCollectSeq('');
      setCollectRemark('');
      setCollectOpen(false);
      await loadStickers(stickerQuery);
    } catch (err) {
      pushToast(formatErrorMessage(err, '收藏消息表情失败'), 'danger');
    } finally {
      if (mountedRef.current) setStickerBusy(false);
    }
  };

  const effectiveVoiceTarget =
    voiceTarget === '__custom__' ? voiceCustomTarget.trim() : voiceTarget.trim();

  const handleVoiceSend = async (dryRun: boolean) => {
    if (!effectiveVoiceTarget || !/^(group|private):[1-9]\d*$/.test(effectiveVoiceTarget)) {
      pushToast('请提供格式为 group:<正整数群号> 或 private:<正整数QQ号> 的有效发送目标', 'warn');
      return;
    }
    if (!voiceFile.trim()) {
      pushToast('请选择或填写语音文件', 'warn');
      return;
    }
    const rawVol = voiceVolume.trim();
    const vol = rawVol === '' ? 1 : Number(rawVol);
    if (!Number.isFinite(vol) || vol < 0.1 || vol > 4.0) {
      pushToast('音量倍率需在 0.1 到 4.0 之间', 'warn');
      return;
    }
    setVoiceBusy(true);
    try {
      const res = await sendVoice({
        target: effectiveVoiceTarget,
        file: voiceFile.trim(),
        volume: vol,
        loudness: voiceLoudness,
        normalize: voiceNormalize,
        dryRun,
        verify: voiceVerify,
        readback: voiceReadback,
      });
      if (!mountedRef.current) return;
      setVoiceResult(res);
      pushToast(dryRun ? '语音预检通过' : '已测试发送语音', 'success');
    } catch (err) {
      pushToast(formatErrorMessage(err, dryRun ? '语音预检失败' : '发送语音失败'), 'danger');
    } finally {
      if (mountedRef.current) setVoiceBusy(false);
    }
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader
          title={
            <span className="flex items-center gap-2">
              <ImageIcon className="h-4 w-4 text-[var(--accent)]" />
              <span>QQ 表情包库</span>
              <Badge tone="neutral">共 {stickerTotal} 个</Badge>
            </span>
          }
          subtitle={`最近同步：${formatDateTimeMs(stickerSyncedAt)}。AI 主动发表情仅在二代仿真回合中通过 agent token 执行，管理员可在此同步、预览、标注备注与收藏。`}
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <select
                value={previewKey}
                aria-label="选择表情包预览鉴权会话"
                onChange={(e) => setPreviewKey(e.target.value)}
                className="h-7 rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2 text-xs"
              >
                <option value="">选择预览鉴权会话…</option>
                {allowlistedDestinations.map((key) => {
                  const found = conversations.find((c) => c.key === key);
                  return (
                    <option key={key} value={key}>
                      {found?.name ? `${found.name} (${key})` : key}
                    </option>
                  );
                })}
              </select>
              <Button size="sm" variant="secondary" onClick={() => setCollectOpen((v) => !v)}>
                <Plus className="h-3.5 w-3.5" />
                收藏消息表情
              </Button>
              <Button size="sm" variant="primary" disabled={stickerBusy} onClick={() => void handleSyncStickers()}>
                <RefreshCw className={`h-3.5 w-3.5 ${stickerBusy ? 'animate-spin' : ''}`} />
                同步 QQ 表情包
              </Button>
            </div>
          }
        />

        <div className="space-y-3 p-4 text-xs">
          {collectOpen && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3">
              <form className="grid grid-cols-1 gap-2 sm:grid-cols-4" onSubmit={(e) => void handleCollect(e)}>
                <select
                  value={collectKey}
                  aria-label="选择收藏表情来源会话"
                  onChange={(e) => setCollectKey(e.target.value)}
                  className={INPUT_CLASS}
                >
                  {allowlistedDestinations.map((key) => {
                    const found = conversations.find((c) => c.key === key);
                    return (
                      <option key={key} value={key}>
                        {found?.name ? `${found.name} (${key})` : key}
                      </option>
                    );
                  })}
                </select>
                <input
                  type="text"
                  value={collectMessageId}
                  onChange={(e) => setCollectMessageId(e.target.value)}
                  placeholder="messageId（二选一）"
                  className={INPUT_CLASS}
                />
                <input
                  type="text"
                  value={collectSeq}
                  onChange={(e) => setCollectSeq(e.target.value)}
                  placeholder="消息 seq（二选一）"
                  className={INPUT_CLASS}
                />
                <div className="flex gap-1.5">
                  <input
                    type="text"
                    value={collectRemark}
                    onChange={(e) => setCollectRemark(e.target.value)}
                    placeholder={`备注（≤ ${collectRemarkMaxChars} 字符，可选）`}
                    className={INPUT_CLASS}
                  />
                  <Button size="sm" variant="primary" type="submit" disabled={stickerBusy}>
                    收藏
                  </Button>
                </div>
              </form>
            </Card>
          )}

          <form
            className="flex flex-wrap items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              void loadStickers(stickerQuery);
            }}
          >
            <div className="min-w-[200px] max-w-sm flex-1">
              <SearchInput value={stickerQuery} onChange={setStickerQuery} placeholder="按备注、标签或用途筛选…" />
            </div>
            <Button size="sm" variant="secondary" type="submit" disabled={stickersLoading}>
              搜索
            </Button>
          </form>

          {editingSticker && (
            <Card className="border-[var(--accent)] bg-[var(--accent-soft)]/25 p-3">
              <form className="space-y-2.5" onSubmit={(e) => void handleSaveStickerEdit(e)}>
                <div className="font-semibold text-[var(--text-primary)]">
                  编辑表情 {editingSticker.id}
                </div>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  <input
                    type="text"
                    maxLength={200}
                    value={editingSticker.note}
                    onChange={(e) =>
                      setEditingSticker((prev) => (prev ? { ...prev, note: e.target.value } : prev))
                    }
                    placeholder="本地备注 (localNote, ≤ 200)"
                    className={INPUT_CLASS}
                  />
                  <input
                    type="text"
                    maxLength={200}
                    value={editingSticker.usage}
                    onChange={(e) =>
                      setEditingSticker((prev) => (prev ? { ...prev, usage: e.target.value } : prev))
                    }
                    placeholder="适用场景 (usage, ≤ 200)"
                    className={INPUT_CLASS}
                  />
                </div>

                <div className="space-y-1.5">
                  <div className="flex flex-wrap gap-1.5">
                    {editingSticker.tags.map((tag, idx) => (
                      <span
                        key={`${tag}-${idx}`}
                        className="inline-flex items-center gap-1 rounded border border-[var(--border-hairline)] bg-[var(--bg-surface)] px-2 py-0.5 text-[11px]"
                      >
                        <span>{tag}</span>
                        <button
                          type="button"
                          aria-label={`移除标签 ${tag}`}
                          onClick={() =>
                            setEditingSticker((prev) =>
                              prev
                                ? { ...prev, tags: prev.tags.filter((_, i) => i !== idx) }
                                : prev,
                            )
                          }
                          className="text-[var(--text-muted)] hover:text-[var(--danger)]"
                        >
                          <X className="h-3 w-3" />
                        </button>
                      </span>
                    ))}
                  </div>
                  <div className="flex gap-1.5">
                    <input
                      type="text"
                      disabled={editingSticker.tags.length >= 20}
                      value={editingSticker.tagDraft}
                      onChange={(e) =>
                        setEditingSticker((prev) =>
                          prev ? { ...prev, tagDraft: e.target.value } : prev,
                        )
                      }
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          e.preventDefault();
                          handleAddTag();
                        }
                      }}
                      placeholder="添加标签（最多 20 个，每个 ≤ 24 字符，支持空格）"
                      aria-label="新增表情标签"
                      className={INPUT_CLASS}
                    />
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={editingSticker.tags.length >= 20 || !editingSticker.tagDraft.trim()}
                      onClick={handleAddTag}
                    >
                      添加标签
                    </Button>
                  </div>
                </div>

                <div className="flex flex-wrap items-center justify-between gap-2 border-t border-[var(--border-hairline)] pt-2">
                  <div className="flex flex-1 items-center gap-2">
                    <input
                      type="text"
                      value={editingSticker.remark}
                      onChange={(e) =>
                        setEditingSticker((prev) =>
                          prev ? { ...prev, remark: e.target.value } : prev,
                        )
                      }
                      placeholder="QQ 远端表情备注（≤ 50 字符）"
                      aria-label="QQ 远端表情备注"
                      className={`${INPUT_CLASS} max-w-xs`}
                    />
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={stickerBusy}
                      onClick={() => void handleSaveQqRemark()}
                    >
                      更新 QQ 备注
                    </Button>
                  </div>
                  <div className="flex items-center gap-1.5">
                    <Button size="sm" variant="ghost" onClick={() => setEditingSticker(null)}>
                      关闭
                    </Button>
                    <Button size="sm" variant="primary" type="submit" disabled={stickerBusy}>
                      保存本地备注
                    </Button>
                  </div>
                </div>
              </form>
            </Card>
          )}

          {previewSticker && (
            <Card className="bg-[var(--bg-subtle)]/40 p-3">
              <div className="mb-2 flex items-center justify-between">
                <span className="font-medium text-[var(--text-primary)]">
                  图像预览：{previewSticker.sticker.desc || previewSticker.sticker.id}
                </span>
                <Button size="sm" variant="ghost" onClick={() => setPreviewSticker(null)}>
                  关闭预览
                </Button>
              </div>
              <div className="flex flex-wrap gap-3">
                {(previewSticker.images?.length ? previewSticker.images : [previewSticker.image]).map(
                  (frame, idx) => (
                    <div key={idx} className="space-y-1 text-center">
                      <img
                        src={`data:${frame.mimeType};base64,${frame.data}`}
                        alt={previewSticker.sticker.desc || previewSticker.sticker.id}
                        className="max-h-36 max-w-36 rounded border border-[var(--border-hairline)] object-contain"
                      />
                      {frame.frameIndex != null && (
                        <div className="text-[10px] text-[var(--text-muted)]">帧 #{frame.frameIndex}</div>
                      )}
                    </div>
                  ),
                )}
              </div>
            </Card>
          )}

          {stickersError && (
            <div className="rounded border border-rose-300 bg-[var(--danger-soft)] p-3 text-[var(--danger)]">
              {stickersError}
            </div>
          )}

          {!stickersLoading && stickers.length === 0 ? (
            <EmptyState
              title="暂无匹配表情包"
              description="可点击右上角「同步 QQ 表情包」从 OneBot 拉取收藏列表。"
            />
          ) : (
            <div className="grid grid-cols-1 gap-2.5 sm:grid-cols-2 lg:grid-cols-3">
              {stickers.map((st) => (
                <div
                  key={st.id}
                  className="flex flex-col justify-between rounded-md border border-[var(--border-hairline)] p-3"
                >
                  <div className="space-y-1">
                    <div className="flex items-center justify-between gap-2">
                      <span className="truncate font-semibold text-[var(--text-primary)]">
                        {st.desc || st.localNote || st.id}
                      </span>
                      <Badge tone="neutral">用过 {st.useCount}</Badge>
                    </div>
                    <div className="font-mono text-[10.5px] text-[var(--text-muted)]">{st.id}</div>
                    {st.localNote && (
                      <div className="text-[11px] text-[var(--text-secondary)]">备注：{st.localNote}</div>
                    )}
                    {st.usage && (
                      <div className="text-[11px] text-[var(--text-muted)]">用法：{st.usage}</div>
                    )}
                    {st.tags.length > 0 && (
                      <div className="flex flex-wrap gap-1 pt-0.5">
                        {st.tags.map((tag) => (
                          <Badge key={tag} tone="accent">
                            {tag}
                          </Badge>
                        ))}
                      </div>
                    )}
                  </div>

                  <div className="mt-2.5 flex items-center justify-end gap-1.5 border-t border-[var(--border-hairline)] pt-2">
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={previewLoading}
                      onClick={() => void handlePreviewSticker(st.id)}
                    >
                      预览图像
                    </Button>
                    <Button
                      size="sm"
                      variant="secondary"
                      onClick={() =>
                        setEditingSticker({
                          id: st.id,
                          note: st.localNote,
                          usage: st.usage,
                          tags: [...st.tags],
                          tagDraft: '',
                          remark: st.desc,
                          originalRemark: st.desc,
                        })
                      }
                    >
                      编辑备注
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </Card>

      <Card>
        <CardHeader
          title={
            <span className="flex items-center gap-2">
              <Volume2 className="h-4 w-4 text-[var(--accent)]" />
              <span>语音库与管理员测试发送</span>
              {voiceData && <Badge tone="neutral">{voiceData.count} 个文件</Badge>}
            </span>
          }
          subtitle={voiceData?.dir ? `语音目录：${voiceData.dir}` : '探测本地音频文件并验证 OneBot PTT 发送链路。'}
          actions={
            <Button size="sm" variant="secondary" disabled={voicesLoading} onClick={() => void loadVoices(true)}>
              <RefreshCw className={`h-3.5 w-3.5 ${voicesLoading ? 'animate-spin' : ''}`} />
              刷新并探测
            </Button>
          }
        />

        <div className="space-y-4 p-4 text-xs">
          {voicesError && (
            <div className="rounded border border-rose-300 bg-[var(--danger-soft)] p-3 text-[var(--danger)]">
              {voicesError}
            </div>
          )}

          {voiceData && (
            <div className="space-y-3">
              <div className="flex flex-wrap gap-3 text-[11px] text-[var(--text-muted)]">
                <span>上限：{voiceData.limits.maxSeconds} 秒</span>
                <span>单文件 ≤ {(voiceData.limits.maxBytes / (1024 * 1024)).toFixed(1)} MB</span>
                <span>每分钟 {voiceData.limits.maxPerMinute} 次</span>
                <span>每小时 {voiceData.limits.maxPerHour} 次</span>
              </div>

              {voiceData.voices.length === 0 ? (
                <EmptyState title="语音目录为空" description="将音频文件放入语音目录后点击「刷新并探测」。" />
              ) : (
                <div className="overflow-x-auto rounded-md border border-[var(--border-hairline)]">
                  <table className="w-full border-collapse text-left text-xs">
                    <thead className="bg-[var(--bg-subtle)] text-[11px] text-[var(--text-muted)]">
                      <tr>
                        <th className="px-3 py-1.5 font-medium">文件名</th>
                        <th className="px-3 py-1.5 font-medium">探测状态</th>
                        <th className="px-3 py-1.5 font-medium">时长</th>
                        <th className="px-3 py-1.5 font-medium">大小</th>
                        <th className="px-3 py-1.5 font-medium">详情</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y divide-[var(--border-hairline)]">
                      {voiceData.voices.map((v) => (
                        <tr
                          key={v.name}
                          tabIndex={0}
                          onClick={() => setVoiceFile(v.name)}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter' || e.key === ' ') {
                              e.preventDefault();
                              setVoiceFile(v.name);
                            }
                          }}
                          className="cursor-pointer hover:bg-[var(--bg-hover)]/60 focus:bg-[var(--bg-hover)]/60 focus:outline-none"
                        >
                          <td className="px-3 py-1.5 font-mono">{v.name}</td>
                          <td className="px-3 py-1.5">
                            <Badge tone={v.ok === false ? 'danger' : 'success'}>
                              {v.ok === false ? '异常' : '可用'}
                            </Badge>
                          </td>
                          <td className="px-3 py-1.5 tabular-nums">
                            {typeof v.seconds === 'number' ? `${v.seconds.toFixed(1)} s` : '—'}
                          </td>
                          <td className="px-3 py-1.5 tabular-nums">
                            {typeof v.bytes === 'number' ? `${(v.bytes / 1024).toFixed(1)} KB` : '—'}
                          </td>
                          <td className="px-3 py-1.5 text-[11px] text-[var(--text-muted)]">
                            {v.error || v.detail || '—'}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </div>
          )}

          <Card className="bg-[var(--bg-subtle)]/35 p-3">
            <div className="mb-2 font-semibold text-[var(--text-primary)]">
              管理员语音预检与测试发送 (/api/voice/send)
            </div>
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-2 lg:grid-cols-4">
              <select
                value={voiceTarget}
                aria-label="选择语音发送目标会话"
                onChange={(e) => setVoiceTarget(e.target.value)}
                className={INPUT_CLASS}
              >
                {allowlistedDestinations.map((key) => {
                  const found = conversations.find((c) => c.key === key);
                  return (
                    <option key={key} value={key}>
                      {found?.name ? `${found.name} (${key})` : key}
                    </option>
                  );
                })}
                <option value="__custom__">手动输入允许会话 Key…</option>
              </select>
              {voiceTarget === '__custom__' && (
                <input
                  type="text"
                  value={voiceCustomTarget}
                  onChange={(e) => setVoiceCustomTarget(e.target.value)}
                  placeholder="group:<正整数> 或 private:<正整数>"
                  aria-label="手动输入语音目标会话"
                  className={INPUT_CLASS}
                />
              )}
              <input
                type="text"
                value={voiceFile}
                onChange={(e) => setVoiceFile(e.target.value)}
                placeholder="音频文件名或本地路径 *"
                aria-label="语音文件名或路径"
                className={INPUT_CLASS}
              />
              <input
                type="number"
                step="0.1"
                min={0.1}
                max={4.0}
                value={voiceVolume}
                onChange={(e) => setVoiceVolume(e.target.value)}
                placeholder="音量倍率 (0.1–4.0)"
                aria-label="音量倍率"
                className={INPUT_CLASS}
              />
              <select
                value={voiceLoudness}
                aria-label="目标响度档位"
                onChange={(e) => setVoiceLoudness(e.target.value as 'quiet' | 'normal' | 'loud')}
                className={INPUT_CLASS}
              >
                <option value="quiet">响度：轻柔 (quiet)</option>
                <option value="normal">响度：标准 (normal)</option>
                <option value="loud">响度：响亮 (loud)</option>
              </select>
            </div>

            <div className="mt-2.5 flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap items-center gap-4 text-[11px] text-[var(--text-secondary)]">
                <label className="inline-flex items-center gap-1.5">
                  <input
                    type="checkbox"
                    checked={voiceNormalize}
                    onChange={(e) => setVoiceNormalize(e.target.checked)}
                  />
                  响度均衡 (normalize)
                </label>
                <label className="inline-flex items-center gap-1.5">
                  <input
                    type="checkbox"
                    checked={voiceVerify}
                    onChange={(e) => setVoiceVerify(e.target.checked)}
                  />
                  发送后校验 (verify)
                </label>
                <label className="inline-flex items-center gap-1.5">
                  <input
                    type="checkbox"
                    checked={voiceReadback}
                    onChange={(e) => setVoiceReadback(e.target.checked)}
                  />
                  回读转写 (readback)
                </label>
              </div>
              <div className="flex items-center gap-2">
                <Button size="sm" variant="secondary" disabled={voiceBusy} onClick={() => void handleVoiceSend(true)}>
                  仅预检 (dryRun)
                </Button>
                <Button size="sm" variant="primary" disabled={voiceBusy} onClick={() => void handleVoiceSend(false)}>
                  <Send className="h-3.5 w-3.5" />
                  测试发送
                </Button>
              </div>
            </div>

            {voiceResult && (
              <div className="mt-2.5 space-y-1 rounded border border-[var(--border-hairline)] bg-[var(--bg-surface)] p-2.5 text-[11px]">
                <div>
                  结果：{voiceResult.dryRun ? '预检通过' : '已发送'} · {voiceResult.codec} ·{' '}
                  {voiceResult.duration.toFixed(1)} s · {voiceResult.detail}
                  {!voiceResult.dryRun && (
                    <>
                      {' · '}
                      {voiceResult.verified ? '回查已确认' : '未回查确认'}
                      {voiceResult.messageId != null ? ` (messageId=${voiceResult.messageId})` : ''}
                    </>
                  )}
                </div>
                {voiceResult.readback && (
                  <div className="text-[var(--text-secondary)]">回读：{voiceResult.readback}</div>
                )}
                {voiceResult.warnings?.length > 0 && (
                  <div className="text-[var(--warn)]">
                    提示：{voiceResult.warnings.join('；')}
                  </div>
                )}
              </div>
            )}
          </Card>
        </div>
      </Card>

      <Card>
        <CardHeader
          title="表情包与语音实时参数 (/api/settings)"
          subtitle="所有开关与限制直接连接设置注册表；修改后通过底部保存栏提交。"
        />
        <div className="grid grid-cols-1 gap-4 p-4 md:grid-cols-2 xl:grid-cols-3">
          {mediaSettings.map((setting) => {
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
                  onReset={() => handleResetMediaSetting(setting.key)}
                />
              </div>
            );
          })}
        </div>
      </Card>
    </div>
  );
};
