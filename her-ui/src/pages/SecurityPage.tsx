import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  Lock,
  RefreshCw,
  ShieldAlert,
  ShieldCheck,
  Terminal,
  Trash2,
} from 'lucide-react';
import {
  ApiRequestError,
  isDesktopOrigin,
  resetSettingValue,
  saveSettingValue,
} from '../api/client';
import {
  BrowserSessionEntry,
  MemoryAclRow,
  SecurityStateResponse,
  WhitelistGroupList,
  WhitelistResponse,
  changePassword,
  fetchBrowserSessions,
  fetchMemoryAcl,
  fetchSecurityState,
  fetchWhitelist,
  rehardenStateDir,
  revokeBrowserSession,
  saveSecuritySwitches,
} from '../api/security';
import { EmptyState } from '../components/ConsoleWidgets';
import {
  Badge,
  Button,
  Card,
  CardHeader,
  PageHeader,
  Switch,
} from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';
import { formatDateTimeMs, formatErrorMessage } from '../utils/format';
import { AccessToolsSection } from './security/AccessToolsSection';
import { WhitelistCard, WhitelistDraft } from './security/WhitelistCard';

function pickPrivateList(group?: WhitelistGroupList): string[] | undefined {
  return group?.private ?? group?.privates;
}

function pickGroupList(group?: WhitelistGroupList): string[] | undefined {
  return group?.groups ?? group?.group;
}

function isSingleValidQqId(raw: string): boolean {
  return /^[1-9]\d*$/.test(raw) && Number.isSafeInteger(Number(raw));
}

// Match Rust str::trim() Unicode White_Space (excluding BOM U+FEFF which JS String.trim strips).
function rustTrim(raw: string): string {
  return raw.replace(
    /^[ \t\n\v\f\r\u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]+|[ \t\n\v\f\r\u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]+$/g,
    '',
  );
}

function formatServerList(values: unknown[] | undefined): { text: string; ignoredCount: number } {
  if (!Array.isArray(values)) return { text: '', ignoredCount: 0 };
  const seen = new Set<string>();
  const valid: string[] = [];
  let ignoredCount = 0;
  for (const item of values) {
    if (typeof item !== 'string' && typeof item !== 'number') {
      ignoredCount += 1;
      continue;
    }
    const trimmed = rustTrim(String(item));
    if (!isSingleValidQqId(trimmed)) {
      ignoredCount += 1;
      continue;
    }
    if (!seen.has(trimmed)) {
      seen.add(trimmed);
      valid.push(trimmed);
    }
  }
  return { text: valid.join('\n'), ignoredCount };
}

function parseValidatedIdList(
  raw: string,
  fieldLabel: string,
): { ids: string[]; invalid: string[] } {
  const seen = new Set<string>();
  const ids: string[] = [];
  const invalid: string[] = [];
  for (const part of raw.split(/[\r\n,，]+/)) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    if (!isSingleValidQqId(trimmed)) {
      invalid.push(`${fieldLabel}: "${trimmed}"`);
      continue;
    }
    if (!seen.has(trimmed)) {
      seen.add(trimmed);
      ids.push(trimmed);
    }
  }
  return { ids, invalid };
}

function isWhitelistFieldEqual(field: keyof WhitelistDraft, a: string, b: string): boolean {
  if (field === 'ownerQQ') {
    return a.trim() === b.trim();
  }
  const parsedA = parseValidatedIdList(a, '');
  const parsedB = parseValidatedIdList(b, '');
  if (parsedA.invalid.length === 0 && parsedB.invalid.length === 0) {
    return (
      parsedA.ids.length === parsedB.ids.length &&
      parsedA.ids.every((id, index) => id === parsedB.ids[index])
    );
  }
  return a.trim() === b.trim();
}

function getDirtyWhitelistFields(
  baseline: WhitelistDraft | null,
  dirty: Partial<WhitelistDraft>,
): (keyof WhitelistDraft)[] {
  if (!baseline) return [];
  return (Object.keys(dirty) as (keyof WhitelistDraft)[]).filter(
    (k) => dirty[k] !== undefined && !isWhitelistFieldEqual(k, dirty[k] as string, baseline[k]),
  );
}

function pruneMatchingWlDirty(
  prev: Partial<WhitelistDraft>,
  baseline: WhitelistDraft | null,
  prevBaseline?: WhitelistDraft | null,
  skipPrevFields?: Set<keyof WhitelistDraft>,
): Partial<WhitelistDraft> {
  if (!baseline) return prev;
  let changed = false;
  const next = { ...prev };
  for (const k of Object.keys(prev) as (keyof WhitelistDraft)[]) {
    const val = next[k];
    if (val === undefined) continue;
    const matchesCurrent = isWhitelistFieldEqual(k, val, baseline[k]);
    const matchesPrev =
      Boolean(prevBaseline) &&
      !skipPrevFields?.has(k) &&
      isWhitelistFieldEqual(k, val, (prevBaseline as WhitelistDraft)[k]);
    if (matchesCurrent || matchesPrev) {
      delete next[k];
      changed = true;
    }
  }
  return changed ? next : prev;
}

type WritePhase = 'restrict' | 'target' | 'relax';

const WRITE_PHASE_RANK: Record<WritePhase, number> = {
  restrict: 0,
  target: 1,
  relax: 2,
};

interface WhitelistSettingWrite {
  field: keyof WhitelistDraft;
  label: string;
  phase: WritePhase;
  sGen: number;
  committedDraft: string;
  run: () => Promise<Record<string, unknown>>;
}

interface DirtyWhitelistSpec {
  dirtyFields: (keyof WhitelistDraft)[];
  writes: WhitelistSettingWrite[];
  invalidEntries: string[];
}

function buildDirtyWhitelistSpec(
  baseline: WhitelistDraft | null,
  dirty: Partial<WhitelistDraft>,
  fieldGens: Partial<Record<keyof WhitelistDraft, number>>,
): DirtyWhitelistSpec {
  const dirtyFields = getDirtyWhitelistFields(baseline, dirty);
  const invalidEntries: string[] = [];
  const writes: WhitelistSettingWrite[] = [];

  const listSettingMap: {
    field: Exclude<keyof WhitelistDraft, 'ownerQQ'>;
    settingKey: string;
    label: string;
    kind: 'allow' | 'deny';
    channel: 'private' | 'groups';
  }[] = [
    {
      field: 'denyPrivate',
      settingKey: 'deny.private',
      label: '拒绝私聊',
      kind: 'deny',
      channel: 'private',
    },
    {
      field: 'denyGroups',
      settingKey: 'deny.groups',
      label: '拒绝群聊',
      kind: 'deny',
      channel: 'groups',
    },
    {
      field: 'allowPrivate',
      settingKey: 'allow.private',
      label: '允许私聊',
      kind: 'allow',
      channel: 'private',
    },
    {
      field: 'allowGroups',
      settingKey: 'allow.groups',
      label: '允许群聊',
      kind: 'allow',
      channel: 'groups',
    },
  ];

  const parsedEntries = listSettingMap
    .filter((item) => dirtyFields.includes(item.field))
    .map((item) => {
      const parsed = parseValidatedIdList(dirty[item.field] ?? '', item.label);
      invalidEntries.push(...parsed.invalid);
      const prevIds = baseline ? parseValidatedIdList(baseline[item.field], '').ids : [];
      const nextIds = parsed.ids;
      const hasAdditions = nextIds.some((id) => !prevIds.includes(id));
      const hasRemovals = prevIds.some((id) => !nextIds.includes(id));
      return {
        ...item,
        prevIds,
        nextIds,
        hasAdditions,
        hasRemovals,
        isMixed: hasAdditions && hasRemovals,
      };
    });

  for (const channel of ['private', 'groups'] as const) {
    const channelAllow = parsedEntries.find(
      (e) => e.channel === channel && e.kind === 'allow',
    );
    const channelDeny = parsedEntries.find(
      (e) => e.channel === channel && e.kind === 'deny',
    );
    if (channelAllow && channelDeny && (channelAllow.isMixed || channelDeny.isMixed)) {
      invalidEntries.push(
        `同一通道（私聊/群聊）同时修改允许列表与拒绝列表且其中包含增删混合改动时无法保证跨接口原子顺序，请先单独保存其中一个列表后再保存另一个`,
      );
    }
  }

  for (const item of parsedEntries) {
    let phase: WritePhase;
    if (item.kind === 'deny') {
      if (item.hasAdditions && !item.hasRemovals) {
        phase = 'restrict';
      } else if (!item.hasAdditions && item.hasRemovals) {
        phase = 'relax';
      } else {
        phase = 'target';
      }
    } else if (
      item.prevIds.length > 0 &&
      item.nextIds.length > 0 &&
      item.hasRemovals &&
      !item.hasAdditions
    ) {
      phase = 'restrict';
    } else {
      phase = 'target';
    }

    const numericIds = item.nextIds.map(Number);
    writes.push({
      field: item.field,
      label: item.settingKey,
      phase,
      sGen: fieldGens[item.field] ?? 0,
      committedDraft: item.nextIds.join('\n'),
      run: () => saveSettingValue(item.settingKey, numericIds, true),
    });
  }

  if (dirtyFields.includes('ownerQQ')) {
    const ownerTrimmed = (dirty.ownerQQ ?? '').trim();
    if (ownerTrimmed && !isSingleValidQqId(ownerTrimmed)) {
      invalidEntries.push(`ownerQQ: "${ownerTrimmed}"`);
    } else {
      const ownerNum = ownerTrimmed ? Number(ownerTrimmed) : null;
      writes.push({
        field: 'ownerQQ',
        label: 'ownerQQ',
        phase: 'target',
        sGen: fieldGens.ownerQQ ?? 0,
        committedDraft: ownerTrimmed,
        run: () =>
          ownerNum !== null
            ? saveSettingValue('ownerQQ', ownerNum, true)
            : resetSettingValue('ownerQQ', true),
      });
    }
  }

  writes.sort((a, b) => WRITE_PHASE_RANK[a.phase] - WRITE_PHASE_RANK[b.phase]);

  return {
    dirtyFields,
    writes,
    invalidEntries,
  };
}

function whitelistToDraft(data: WhitelistResponse): {
  draft: WhitelistDraft;
  ignoredCount: number;
} {
  const allowPriv = formatServerList(pickPrivateList(data.allow));
  const allowGrp = formatServerList(pickGroupList(data.allow));
  const denyPriv = formatServerList(pickPrivateList(data.deny));
  const denyGrp = formatServerList(pickGroupList(data.deny));
  let ownerStr = '';
  let ownerIgnored = 0;
  if (data.ownerQQ !== null && data.ownerQQ !== undefined) {
    const rawOwner = rustTrim(String(data.ownerQQ));
    if (rawOwner) {
      if (
        /^\d+$/.test(rawOwner) &&
        Number(rawOwner) > 0 &&
        Number.isSafeInteger(Number(rawOwner))
      ) {
        ownerStr = String(Number(rawOwner));
      } else {
        ownerIgnored = 1;
      }
    }
  }
  return {
    draft: {
      allowPrivate: allowPriv.text,
      allowGroups: allowGrp.text,
      denyPrivate: denyPriv.text,
      denyGroups: denyGrp.text,
      ownerQQ: ownerStr,
    },
    ignoredCount:
      allowPriv.ignoredCount +
      allowGrp.ignoredCount +
      denyPriv.ignoredCount +
      denyGrp.ignoredCount +
      ownerIgnored,
  };
}

export const SecurityPage: React.FC = () => {
  const {
    authStatus,
    settings,
    navigate,
    openConfirm,
    pushToast,
    refreshAuthAndShell,
    refreshLiveData,
    setDirtyEntry,
  } = useConsole();

  const [loading, setLoading] = useState(true);
  const [sessions, setSessions] = useState<BrowserSessionEntry[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [securityState, setSecurityState] = useState<SecurityStateResponse | null>(null);
  const [securityError, setSecurityError] = useState<string | null>(null);
  const [whitelistBaseline, setWhitelistBaseline] = useState<WhitelistDraft | null>(null);
  const [whitelistIgnoredCount, setWhitelistIgnoredCount] = useState(0);
  const [whitelistError, setWhitelistError] = useState<string | null>(null);
  const [wlDirty, setWlDirty] = useState<Partial<WhitelistDraft>>({});
  const [aclRows, setAclRows] = useState<MemoryAclRow[]>([]);
  const [aclError, setAclError] = useState<string | null>(null);

  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [pwBusy, setPwBusy] = useState(false);

  const [switchBusy, setSwitchBusy] = useState<string | null>(null);
  const [rehardenBusy, setRehardenBusy] = useState(false);
  const [wlSaving, setWlSaving] = useState(false);

  const mountedRef = useRef(true);
  const activeLoadsRef = useRef(0);
  const loadSeqRef = useRef(0);
  const wlGenRef = useRef(0);
  const aclGenRef = useRef(0);
  const secGenRef = useRef(0);
  const wlFieldGenRef = useRef<Partial<Record<keyof WhitelistDraft, number>>>({});
  const wlSavingRef = useRef(false);
  const wlBaselineRef = useRef<WhitelistDraft | null>(null);
  const wlDirtyRef = useRef<Partial<WhitelistDraft>>({});

  const applyWhitelistBaseline = useCallback((nextBaseline: WhitelistDraft) => {
    wlBaselineRef.current = nextBaseline;
    setWhitelistBaseline(nextBaseline);
  }, []);

  const updateWlDirty = useCallback(
    (updater: (prev: Partial<WhitelistDraft>) => Partial<WhitelistDraft>) => {
      const next = updater(wlDirtyRef.current);
      if (next !== wlDirtyRef.current) {
        wlDirtyRef.current = next;
        setWlDirty(next);
      }
    },
    [],
  );

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      setDirtyEntry(null);
    };
  }, [setDirtyEntry]);

  const loadAll = useCallback(async () => {
    const seq = ++loadSeqRef.current;
    const startWlGen = wlGenRef.current;
    const startAclGen = aclGenRef.current;
    const startSecGen = secGenRef.current;
    activeLoadsRef.current += 1;
    setLoading(true);
    try {
      const [sessRes, secRes, wlRes, aclRes] = await Promise.allSettled([
        fetchBrowserSessions(),
        fetchSecurityState(),
        fetchWhitelist(),
        fetchMemoryAcl(),
      ]);
      if (!mountedRef.current || seq !== loadSeqRef.current) return;

      if (sessRes.status === 'fulfilled') {
        setSessions(sessRes.value.sessions);
        setSessionsError(null);
      } else {
        setSessionsError(formatErrorMessage(sessRes.reason, '读取浏览器会话失败'));
      }

      if (secRes.status === 'fulfilled') {
        if (secGenRef.current === startSecGen) {
          setSecurityState(secRes.value);
          setSecurityError(null);
        }
      } else if (secGenRef.current === startSecGen) {
        setSecurityError(formatErrorMessage(secRes.reason, '读取安全状态失败'));
      }

      if (wlRes.status === 'fulfilled') {
        if (wlGenRef.current === startWlGen) {
          const prevBaseline = wlBaselineRef.current;
          const { draft: nextBaseline, ignoredCount } = whitelistToDraft(wlRes.value);
          applyWhitelistBaseline(nextBaseline);
          setWhitelistIgnoredCount(ignoredCount);
          setWhitelistError(null);
          if (!wlSavingRef.current) {
            updateWlDirty((prev) => pruneMatchingWlDirty(prev, nextBaseline, prevBaseline));
          }
        }
      } else if (wlGenRef.current === startWlGen) {
        setWhitelistError(formatErrorMessage(wlRes.reason, '读取白名单失败'));
      }

      if (aclRes.status === 'fulfilled') {
        if (aclGenRef.current === startAclGen) {
          setAclRows(aclRes.value.acl);
          setAclError(null);
        }
      } else if (aclGenRef.current === startAclGen) {
        setAclError(formatErrorMessage(aclRes.reason, '读取记忆 ACL 失败'));
      }
    } finally {
      activeLoadsRef.current = Math.max(0, activeLoadsRef.current - 1);
      if (mountedRef.current) {
        setLoading(activeLoadsRef.current > 0);
      }
    }
  }, []);

  useEffect(() => {
    void loadAll();
  }, [loadAll]);

  const effectiveWhitelist = useMemo<WhitelistDraft>(
    () => ({
      allowPrivate: wlDirty.allowPrivate ?? whitelistBaseline?.allowPrivate ?? '',
      allowGroups: wlDirty.allowGroups ?? whitelistBaseline?.allowGroups ?? '',
      denyPrivate: wlDirty.denyPrivate ?? whitelistBaseline?.denyPrivate ?? '',
      denyGroups: wlDirty.denyGroups ?? whitelistBaseline?.denyGroups ?? '',
      ownerQQ: wlDirty.ownerQQ ?? whitelistBaseline?.ownerQQ ?? '',
    }),
    [whitelistBaseline, wlDirty],
  );

  const dirtyFieldNames = useMemo(
    () => getDirtyWhitelistFields(whitelistBaseline, wlDirty),
    [whitelistBaseline, wlDirty],
  );

  const whitelistDirty = dirtyFieldNames.length > 0;

  const requirePwEffective = useMemo<boolean | null>(() => {
    const fromState = securityState?.security.requirePasswordLocally;
    if (typeof fromState === 'boolean') return fromState;
    const item = settings.find((s) => s.key === 'security.requirePasswordLocally');
    if (typeof item?.value === 'boolean') return item.value;
    if (typeof authStatus?.requirePasswordLocally === 'boolean') {
      return authStatus.requirePasswordLocally;
    }
    return null;
  }, [authStatus?.requirePasswordLocally, securityState, settings]);

  const commitWhitelist = useCallback(
    async (spec: DirtyWhitelistSpec) => {
      if (wlSavingRef.current) return;
      wlSavingRef.current = true;
      setWlSaving(true);
      const preSaveBaseline = wlBaselineRef.current;
      let untouchedBaseline: WhitelistDraft | null = preSaveBaseline;
      const submittedFields = new Set(spec.writes.map((w) => w.field));
      const savedFields: { field: keyof WhitelistDraft; sGen: number }[] = [];
      const acknowledgedPatch: Partial<WhitelistDraft> = {};
      const abortedDeps: string[] = [];
      const errors: string[] = [];
      const mirrorWarnings: string[] = [];
      let restrictionFailed = false;
      let targetFailed = false;
      let anyWriteDispatched = false;

      // Clear any formatting-only drafts matching the pre-save baseline before starting.
      if (mountedRef.current) {
        updateWlDirty((prev) => pruneMatchingWlDirty(prev, preSaveBaseline));
      }

      try {
        for (const op of spec.writes) {
          if (
            (restrictionFailed && op.phase !== 'restrict') ||
            (targetFailed && op.phase === 'relax')
          ) {
            abortedDeps.push(op.label);
            continue;
          }
          anyWriteDispatched = true;
          try {
            const res = await op.run();
            acknowledgedPatch[op.field] = op.committedDraft;
            savedFields.push({ field: op.field, sGen: op.sGen });
            if (typeof res.mirrorError === 'string' && res.mirrorError) {
              mirrorWarnings.push(`${op.label}: ${res.mirrorError}`);
            }
          } catch (err) {
            errors.push(`${op.label}: ${formatErrorMessage(err, '保存失败')}`);
            if (op.phase === 'restrict') {
              restrictionFailed = true;
            } else if (op.phase === 'target') {
              targetFailed = true;
            }
          }
        }

        if (abortedDeps.length > 0) {
          errors.push(`${abortedDeps.join('、')}: 因前置收紧或准入写入失败而中止后续放宽`);
        }

        if (anyWriteDispatched) {
          wlGenRef.current += 1;
          let refreshedBaseline = wlBaselineRef.current;
          if (mountedRef.current && refreshedBaseline && Object.keys(acknowledgedPatch).length > 0) {
            refreshedBaseline = { ...refreshedBaseline, ...acknowledgedPatch };
            applyWhitelistBaseline(refreshedBaseline);
          }
          try {
            const latestWl = await fetchWhitelist();
            if (mountedRef.current) {
              const parsed = whitelistToDraft(latestWl);
              refreshedBaseline = parsed.draft;
              applyWhitelistBaseline(refreshedBaseline);
              setWhitelistIgnoredCount(parsed.ignoredCount);
              setWhitelistError(null);
            }
          } catch (refreshErr) {
            if (mountedRef.current) {
              setWhitelistError(formatErrorMessage(refreshErr, '保存后刷新白名单失败'));
            }
          }

          if (mountedRef.current) {
            updateWlDirty((prev) => {
              const next = { ...prev };
              for (const { field, sGen } of savedFields) {
                if ((wlFieldGenRef.current[field] ?? 0) === sGen) {
                  delete next[field];
                }
              }
              return pruneMatchingWlDirty(next, refreshedBaseline, untouchedBaseline, submittedFields);
            });
            // Subsequent edits after post-GET display must never be pruned against preSaveBaseline.
            untouchedBaseline = refreshedBaseline;
          }

          await refreshLiveData();
        }

        if (errors.length > 0) {
          pushToast(`部分白名单配置保存失败：${errors.join('；')}`, 'danger');
        } else if (savedFields.length > 0) {
          pushToast('白名单与主人配置已保存', 'success');
        }
        if (mirrorWarnings.length > 0) {
          pushToast(
            `黑白名单及主人设置已存入本地配置库，但同步到主配置文件未成：${mirrorWarnings.join('；')}`,
            'warn',
          );
        }
      } finally {
        wlSavingRef.current = false;
        if (mountedRef.current) {
          setWlSaving(false);
          updateWlDirty((prev) =>
            pruneMatchingWlDirty(prev, wlBaselineRef.current, untouchedBaseline, submittedFields),
          );
        }
      }
    },
    [applyWhitelistBaseline, pushToast, refreshLiveData, updateWlDirty],
  );

  const requestSaveWhitelist = useCallback(async () => {
    if (!wlBaselineRef.current || wlSavingRef.current) return;
    const initial = buildDirtyWhitelistSpec(
      wlBaselineRef.current,
      wlDirtyRef.current,
      wlFieldGenRef.current,
    );
    if (initial.dirtyFields.length === 0) return;

    if (initial.invalidEntries.length > 0) {
      pushToast(`存在非有效正整数 QQ/群号，未保存：${initial.invalidEntries.join('；')}`, 'warn');
      return;
    }

    openConfirm({
      title: '确认更新白名单与主人账号？',
      description:
        '将仅逐项提交本次修改过的白名单列表或主人 QQ 字段（未修改的列表与别名配置保持不变），保存后立即影响消息准入控制。',
      confirmLabel: '保存白名单',
      danger: true,
      onConfirm: async () => {
        const live = buildDirtyWhitelistSpec(
          wlBaselineRef.current,
          wlDirtyRef.current,
          wlFieldGenRef.current,
        );
        if (live.dirtyFields.length === 0) {
          pushToast('白名单改动已与最新服务端状态一致，无需保存', 'info');
          return;
        }
        if (live.invalidEntries.length > 0) {
          pushToast(`存在非有效正整数 QQ/群号，未保存：${live.invalidEntries.join('；')}`, 'warn');
          return;
        }
        await commitWhitelist(live);
      },
    });
  }, [commitWhitelist, openConfirm, pushToast]);

  const discardWhitelist = useCallback(() => {
    if (wlSavingRef.current) return;
    for (const k of dirtyFieldNames) {
      wlFieldGenRef.current[k] = (wlFieldGenRef.current[k] ?? 0) + 1;
    }
    updateWlDirty(() => ({}));
  }, [dirtyFieldNames, updateWlDirty]);

  useEffect(() => {
    if (!whitelistDirty) {
      setDirtyEntry(null);
      return;
    }
    setDirtyEntry({
      count: dirtyFieldNames.length,
      label: `白名单 / 主人配置（${dirtyFieldNames.length} 项未保存）`,
      onSave: requestSaveWhitelist,
      onDiscard: discardWhitelist,
    });
  }, [
    dirtyFieldNames.length,
    discardWhitelist,
    requestSaveWhitelist,
    setDirtyEntry,
    whitelistDirty,
  ]);

  const updateWlField = (field: keyof WhitelistDraft, value: string) => {
    if (wlSavingRef.current) return;
    wlFieldGenRef.current[field] = (wlFieldGenRef.current[field] ?? 0) + 1;
    updateWlDirty((prev) => {
      const next = { ...prev, [field]: value };
      if (
        !wlSavingRef.current &&
        wlBaselineRef.current &&
        value === wlBaselineRef.current[field]
      ) {
        delete next[field];
      }
      return next;
    });
  };

  const handleChangePassword = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!authStatus?.passwordSet) {
      pushToast('当前尚未设定管理员密码，请先前往 /setup 完成首次设密', 'warn');
      return;
    }
    if (!currentPassword) {
      pushToast('请输入当前密码', 'warn');
      return;
    }
    if (!newPassword.trim()) {
      pushToast('请输入新密码', 'warn');
      return;
    }
    if (newPassword !== confirmPassword) {
      pushToast('两次输入的新密码不一致', 'warn');
      return;
    }
    setPwBusy(true);
    try {
      const res = await changePassword(currentPassword, newPassword);
      if (!mountedRef.current) return;
      setCurrentPassword('');
      setNewPassword('');
      setConfirmPassword('');
      pushToast(
        res.sessionRotated ? '密码已更新，当前会话凭证已轮换' : '密码已更新',
        'success',
      );
      await Promise.all([refreshAuthAndShell(), loadAll()]);
    } catch (err) {
      pushToast(formatErrorMessage(err, '修改密码失败'), 'danger');
      if (err instanceof ApiRequestError && err.status === 401 && !err.message.includes('当前密码')) {
        void refreshAuthAndShell();
      }
    } finally {
      if (mountedRef.current) setPwBusy(false);
    }
  };

  const toggleSecuritySwitch = (
    key: 'interceptNotify' | 'outboundGuard' | 'requirePasswordLocally',
    next: boolean,
  ) => {
    const labelMap = {
      interceptNotify: '出站拦截后通知主人 (interceptNotify)',
      outboundGuard: '出站防护 (outboundGuard)',
      requirePasswordLocally: '本机访问仍要求管理员密码 (security.requirePasswordLocally)',
    };
    const warnExtra =
      key === 'requirePasswordLocally' && !next
        ? '\n注意：关闭后本机回环访问可免密进入控制台，请仅在可信本机环境使用。'
        : '';
    openConfirm({
      title: `确认${next ? '开启' : '关闭'}「${labelMap[key]}」？`,
      description: `安全策略开关变更将立即生效并写入审计日志。${warnExtra}`,
      confirmLabel: '确认修改',
      danger: true,
      onConfirm: async () => {
        setSwitchBusy(key);
        try {
          const res = await saveSecuritySwitches({ [key]: next, confirm: true });
          secGenRef.current += 1;
          if (mountedRef.current) {
            setSecurityState((prev) => (prev ? { ...prev, security: res.security } : prev));
            setSecurityError(null);
          }
          pushToast(`已${next ? '开启' : '关闭'} ${labelMap[key]}`, 'success');
          await Promise.all([refreshAuthAndShell(), refreshLiveData(), loadAll()]);
        } catch (err) {
          pushToast(formatErrorMessage(err, '更新安全开关失败'), 'danger');
        } finally {
          if (mountedRef.current) setSwitchBusy(null);
        }
      },
    });
  };

  const handleReharden = async () => {
    setRehardenBusy(true);
    try {
      const res = await rehardenStateDir();
      if (!mountedRef.current) return;
      if (res.ok && res.hardened) {
        pushToast(`状态目录权限已重新加固：${res.path}`, 'success');
      } else {
        pushToast('状态目录仍未达到完全加固状态，请按下方手工命令处理。', 'warn');
      }
      await loadAll();
    } catch (err) {
      pushToast(formatErrorMessage(err, '重新加固状态目录失败'), 'danger');
    } finally {
      if (mountedRef.current) setRehardenBusy(false);
    }
  };

  const requestRevokeSession = (
    target: { id: string } | { others: true } | { all: true },
    desc: string,
  ) => {
    openConfirm({
      title: '确认吊销浏览器会话？',
      description: desc,
      confirmLabel: '确认吊销',
      danger: true,
      onConfirm: async () => {
        try {
          const res = await revokeBrowserSession(target);
          if (!mountedRef.current) return;
          pushToast(`已吊销 ${res.revoked} 个浏览器会话`, 'success');
          await Promise.all([refreshAuthAndShell(), loadAll()]);
        } catch (err) {
          pushToast(formatErrorMessage(err, '吊销会话失败'), 'danger');
        }
      },
    });
  };

  const inputClass =
    'w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-1.5 text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50';

  return (
    <div className="space-y-5">
      <PageHeader
        title="权限与安全"
        description="管理员密码、浏览器会话管理、私聊与群聊黑白名单、安全开关、状态目录加固与记忆 ACL 授权。"
        actions={
          <Button size="sm" onClick={() => void loadAll()} disabled={loading}>
            <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
            刷新状态
          </Button>
        }
      />

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader
            title="管理员密码与本地访问门槛"
            subtitle="修改管理员密码或切换本机免密访问策略"
            actions={
              <Badge tone={authStatus?.passwordSet ? 'success' : 'warn'}>
                {authStatus?.passwordSet ? '已设密码' : '未设密码'}
              </Badge>
            }
          />
          <div className="space-y-4 p-4">
            <div className="flex items-center justify-between gap-3 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3">
              <div className="min-w-0">
                <div className="text-xs font-medium text-[var(--text-primary)]">
                  本机回环仍需密码登录
                </div>
                <div className="mt-0.5 font-mono text-[11px] text-[var(--text-muted)]">
                  security.requirePasswordLocally
                </div>
              </div>
              <Switch
                checked={requirePwEffective === true}
                disabled={requirePwEffective === null || switchBusy === 'requirePasswordLocally'}
                label="本机回环仍需密码登录"
                onChange={(next) => toggleSecuritySwitch('requirePasswordLocally', next)}
              />
            </div>

            {!authStatus?.passwordSet ? (
              <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-amber-300 bg-[var(--warn-soft)] p-3 text-xs text-[var(--warn)] dark:border-amber-900">
                <div className="space-y-1">
                  <p>
                    当前尚未设定管理员密码；请前往规范浏览器地址的首次设置页完成设密
                    {isDesktopOrigin() ? '（桌面窗口内不支持直接跳转，请在外部浏览器打开）' : ''}：
                  </p>
                  {authStatus?.canonicalOrigin &&
                    typeof window !== 'undefined' &&
                    authStatus.canonicalOrigin !== window.location.origin && (
                      <code className="block break-all rounded bg-[var(--bg-surface)] px-2 py-1 font-mono text-[11px] text-[var(--text-primary)]">
                        {`${authStatus.canonicalOrigin}/setup`}
                      </code>
                    )}
                </div>
                {authStatus?.canonicalOrigin &&
                typeof window !== 'undefined' &&
                authStatus.canonicalOrigin !== window.location.origin ? (
                  <div className="flex flex-wrap items-center gap-2">
                    <Button
                      size="sm"
                      variant="secondary"
                      onClick={async () => {
                        const setupUrl = `${authStatus.canonicalOrigin}/setup`;
                        try {
                          await navigator.clipboard.writeText(setupUrl);
                          pushToast('已复制首次设置地址，请在浏览器中打开', 'success');
                        } catch {
                          pushToast(`请手动复制此地址到浏览器打开：${setupUrl}`, 'info');
                        }
                      }}
                    >
                      复制首次设置地址
                    </Button>
                    {!isDesktopOrigin() && (
                      <a
                        href={`${authStatus.canonicalOrigin}/setup`}
                        rel="noreferrer"
                        className="inline-flex h-7 items-center rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 text-xs font-medium text-[var(--text-primary)] hover:bg-[var(--bg-hover)]"
                      >
                        前往首次设置 (/setup)
                      </a>
                    )}
                  </div>
                ) : (
                  <Button size="sm" variant="secondary" onClick={() => navigate('setup')}>
                    前往首次设置 (/setup)
                  </Button>
                )}
              </div>
            ) : (
              <form onSubmit={(e) => void handleChangePassword(e)} className="space-y-2.5">
                <div>
                  <label className="mb-1 block text-[11px] font-medium text-[var(--text-secondary)]">
                    当前密码
                  </label>
                  <input
                    type="password"
                    autoComplete="current-password"
                    value={currentPassword}
                    onChange={(e) => setCurrentPassword(e.target.value)}
                    className={inputClass}
                    placeholder="输入当前管理员密码"
                  />
                </div>
                <div className="grid gap-2.5 sm:grid-cols-2">
                  <div>
                    <label className="mb-1 block text-[11px] font-medium text-[var(--text-secondary)]">
                      新密码
                    </label>
                    <input
                      type="password"
                      autoComplete="new-password"
                      value={newPassword}
                      onChange={(e) => setNewPassword(e.target.value)}
                      className={inputClass}
                      placeholder="输入新密码"
                    />
                  </div>
                  <div>
                    <label className="mb-1 block text-[11px] font-medium text-[var(--text-secondary)]">
                      确认新密码
                    </label>
                    <input
                      type="password"
                      autoComplete="new-password"
                      value={confirmPassword}
                      onChange={(e) => setConfirmPassword(e.target.value)}
                      className={inputClass}
                      placeholder="再次输入新密码"
                    />
                  </div>
                </div>
                <div className="flex items-center justify-between pt-1">
                  <span className="text-[11px] text-[var(--text-muted)]">
                    认证方式：{authStatus?.authKind || 'unknown'}
                  </span>
                  <Button type="submit" size="sm" variant="primary" disabled={pwBusy}>
                    <Lock className="h-3.5 w-3.5" />
                    {pwBusy ? '更新中…' : '更新密码'}
                  </Button>
                </div>
              </form>
            )}
          </div>
        </Card>

        <Card>
          <CardHeader
            title="安全策略开关与状态目录加固"
            subtitle="出站拦截告警及磁盘目录权限巡检"
            actions={
              securityState ? (
                securityState.stateDir.hardened ? (
                  <Badge tone="success">
                    <ShieldCheck className="h-3 w-3" />
                    目录已加固
                  </Badge>
                ) : (
                  <Badge tone="warn">
                    <ShieldAlert className="h-3 w-3" />
                    需加固检查
                  </Badge>
                )
              ) : null
            }
          />
          <div className="space-y-3 p-4">
            {securityError && !securityState ? (
              <EmptyState
                icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
                title="安全状态读取失败"
                description={securityError}
                action={
                  <Button size="sm" onClick={() => void loadAll()}>
                    重新加载
                  </Button>
                }
              />
            ) : (
              <>
                {securityError && (
                  <div className="flex items-center justify-between gap-2 rounded-md border border-amber-300 bg-[var(--warn-soft)] px-3 py-2 text-[11px] text-[var(--warn)] dark:border-amber-900">
                    <span>最近刷新失败，当前显示缓存值：{securityError}</span>
                    <Button size="sm" variant="ghost" onClick={() => void loadAll()}>
                      重试
                    </Button>
                  </div>
                )}
                <div className="flex items-center justify-between gap-3 rounded-md border border-[var(--border-hairline)] p-3">
                  <div>
                    <div className="text-xs font-medium text-[var(--text-primary)]">
                      出站拦截后通知主人
                    </div>
                    <div className="font-mono text-[11px] text-[var(--text-muted)]">
                      interceptNotify（触发出站拦截后向主人发送提醒）
                    </div>
                  </div>
                  <Switch
                    checked={securityState?.security.interceptNotify ?? false}
                    disabled={!securityState || switchBusy === 'interceptNotify'}
                    label="出站拦截后通知主人"
                    onChange={(next) => toggleSecuritySwitch('interceptNotify', next)}
                  />
                </div>

                <div className="flex items-center justify-between gap-3 rounded-md border border-[var(--border-hairline)] p-3">
                  <div>
                    <div className="text-xs font-medium text-[var(--text-primary)]">
                      出站敏感内容与越权防护
                    </div>
                    <div className="font-mono text-[11px] text-[var(--text-muted)]">outboundGuard</div>
                  </div>
                  <Switch
                    checked={securityState?.security.outboundGuard ?? true}
                    disabled={!securityState || switchBusy === 'outboundGuard'}
                    label="出站敏感内容与越权防护"
                    onChange={(next) => toggleSecuritySwitch('outboundGuard', next)}
                  />
                </div>

                <div className="rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)] p-3 text-xs">
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-medium text-[var(--text-primary)]">状态目录权限</span>
                    <Button size="sm" onClick={() => void handleReharden()} disabled={rehardenBusy}>
                      <Terminal className="h-3.5 w-3.5" />
                      {rehardenBusy ? '加固中…' : '重新加固目录'}
                    </Button>
                  </div>
                  {securityState?.stateDir.path && (
                    <div className="mt-1.5 break-all font-mono text-[11px] text-[var(--text-muted)]">
                      {securityState.stateDir.path}
                    </div>
                  )}
                  {securityState?.stateDir.error && (
                    <p className="mt-1 text-[11px] text-[var(--danger)]">
                      {securityState.stateDir.error}
                    </p>
                  )}
                  {securityState?.stateDir.manualCommand && (
                    <pre className="mt-2 overflow-x-auto rounded border border-[var(--border-hairline)] bg-[var(--bg-surface)] p-2 font-mono text-[10.5px] text-[var(--text-secondary)]">
                      {securityState.stateDir.manualCommand}
                    </pre>
                  )}
                </div>
              </>
            )}
          </div>
        </Card>
      </div>

      <Card>
        <CardHeader
          title={`活跃浏览器会话 (${sessions.length})`}
          subtitle="查看当前颁发的浏览器会话凭证并按需吊销"
          actions={
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                disabled={
                  authStatus?.authKind !== 'browser' ||
                  sessions.filter((s) => !s.current).length === 0
                }
                onClick={() =>
                  requestRevokeSession(
                    { others: true },
                    '将吊销除当前浏览器之外的全部会话，其他终端需重新登录。',
                  )
                }
              >
                注销其他会话
              </Button>
              <Button
                size="sm"
                variant="danger"
                disabled={sessions.length === 0}
                onClick={() =>
                  requestRevokeSession(
                    { all: true },
                    '将吊销全部浏览器会话（包括当前会话），执行后您将被退出登录。',
                  )
                }
              >
                注销全部会话
              </Button>
            </div>
          }
        />
        <div className="p-4">
          {sessionsError ? (
            <EmptyState
              icon={<AlertTriangle className="h-6 w-6 text-[var(--danger)]" />}
              title="浏览器会话列表读取失败"
              description={sessionsError}
              action={
                <Button size="sm" onClick={() => void loadAll()}>
                  重新加载
                </Button>
              }
            />
          ) : sessions.length === 0 ? (
            <EmptyState
              title={loading ? '正在加载浏览器会话…' : '暂无活跃浏览器会话'}
              description="当前未发现有效的浏览器会话记录。"
            />
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full border-collapse text-left text-xs">
                <thead>
                  <tr className="border-b border-[var(--border-hairline)] text-[11px] text-[var(--text-muted)]">
                    <th className="py-2 pr-3 font-medium">会话 ID</th>
                    <th className="py-2 pr-3 font-medium">来源 Origin</th>
                    <th className="py-2 pr-3 font-medium">创建时间</th>
                    <th className="py-2 pr-3 font-medium">最近活跃</th>
                    <th className="py-2 pr-3 font-medium">过期时间</th>
                    <th className="py-2 text-right font-medium">操作</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-[var(--border-hairline)]">
                  {sessions.map((sess) => (
                    <tr key={sess.id}>
                      <td className="py-2 pr-3 font-mono text-[11px]">
                        <span>{sess.id}</span>
                        {sess.current && (
                          <Badge tone="accent" className="ml-1.5">
                            当前会话
                          </Badge>
                        )}
                      </td>
                      <td className="py-2 pr-3 font-mono text-[11px] text-[var(--text-secondary)]">
                        {sess.origin}
                      </td>
                      <td className="py-2 pr-3 text-[var(--text-secondary)]">
                        {formatDateTimeMs(sess.createdMs)}
                      </td>
                      <td className="py-2 pr-3 text-[var(--text-secondary)]">
                        {formatDateTimeMs(sess.lastUsedMs)}
                      </td>
                      <td className="py-2 pr-3 text-[var(--text-secondary)]">
                        {formatDateTimeMs(sess.expiresMs)}
                      </td>
                      <td className="py-2 text-right">
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() =>
                            requestRevokeSession(
                              { id: sess.id },
                              sess.current
                                ? '将吊销当前正在使用的浏览器会话并需要重新登录。'
                                : `将吊销会话 ${sess.id} (${sess.origin})。`,
                            )
                          }
                        >
                          <Trash2 className="h-3.5 w-3.5 text-[var(--danger)]" />
                          吊销
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

      <WhitelistCard
        ready={whitelistBaseline !== null}
        error={whitelistError}
        ignoredCount={whitelistIgnoredCount}
        draft={effectiveWhitelist}
        dirty={whitelistDirty}
        saving={wlSaving}
        onUpdateField={updateWlField}
        onSave={() => void requestSaveWhitelist()}
        onDiscard={discardWhitelist}
        onRetry={() => void loadAll()}
      />

      <AccessToolsSection
        aclRows={aclRows}
        aclError={aclError}
        setAclRows={setAclRows}
        setAclError={setAclError}
        aclGenRef={aclGenRef}
        mountedRef={mountedRef}
        onRetryLoad={() => void loadAll()}
      />
    </div>
  );
};
