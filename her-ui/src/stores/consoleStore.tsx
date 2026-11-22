import React, { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react';
import {
  AuthStatusResponse,
  ConversationStateSummary,
  SettingItemView,
  captureDesktopTokenFromUrl,
  fetchAuthStatus,
  fetchConsoleStatus,
  fetchConversationStates,
  fetchPendingItems,
  fetchQqLoginState,
  fetchReleaseStatus,
  fetchSettingsList,
  isDesktopOrigin,
  resetSettingValue,
  saveSettingValue,
  setUnauthorizedHandler,
  submitLogout,
} from '../api/client';
import {
  InspectorTarget,
  LEGACY_REDIRECTS,
  NAV_ITEMS,
  PageId,
  pageForPath,
  pathForPage,
  sanitizeNextPath,
} from '../catalog/navigation';

export type ThemeMode = 'light' | 'dark';
export type ToastTone = 'info' | 'success' | 'warn' | 'danger';

export interface ToastItem {
  id: string;
  message: string;
  tone: ToastTone;
  undoLabel?: string;
  onUndo?: () => void;
}

export interface ConfirmRequest {
  title: string;
  description: string;
  confirmLabel?: string;
  danger?: boolean;
  onConfirm: () => Promise<void> | void;
}

export interface DirtyEntry {
  count: number;
  label: string;
  onSave: () => Promise<void> | void;
  onDiscard: () => void;
}

interface ConsoleContextValue {
  page: PageId;
  activeTabs: Record<string, string>;
  setActiveTab: (pageId: string, tabId: string) => void;
  navigate: (page: PageId, tabId?: string, query?: string) => void;
  navigatePath: (fullPath: string) => void;
  nextPathAfterAuth: string;
  authStatus: AuthStatusResponse | null;
  authLoading: boolean;
  expiredNotice: string | null;
  clearExpiredNotice: () => void;
  refreshAuthAndShell: () => Promise<void>;
  refreshLiveData: () => Promise<void>;
  signOut: () => Promise<void>;
  theme: ThemeMode;
  toggleTheme: () => void;
  navDrawerOpen: boolean;
  setNavDrawerOpen: (open: boolean) => void;
  paletteOpen: boolean;
  setPaletteOpen: (open: boolean) => void;
  inspector: InspectorTarget;
  setInspector: (target: InspectorTarget) => void;
  setInspectorDirty: (dirty: boolean) => void;
  highlightedKey: string | null;
  jumpToSetting: (key: string, openDrawer?: boolean) => void;
  runState: string;
  globalMode: string;
  pendingCount: number;
  updateAvailable: string | null;
  qqLoginState: string;
  triggerQqLoginWindow: () => void;
  settings: SettingItemView[];
  conversations: ConversationStateSummary[];
  saveSetting: (setting: string, value: unknown, confirm?: boolean) => Promise<void>;
  resetSetting: (setting: string, confirm?: boolean) => Promise<void>;
  toasts: ToastItem[];
  pushToast: (message: string, tone?: ToastTone, undo?: { label: string; run: () => void }) => void;
  dismissToast: (id: string) => void;
  confirmRequest: ConfirmRequest | null;
  openConfirm: (req: ConfirmRequest) => void;
  closeConfirm: () => void;
  dirtyEntry: DirtyEntry | null;
  setDirtyEntry: (entry: DirtyEntry | null) => void;
}

const ConsoleContext = createContext<ConsoleContextValue | null>(null);

function readInitialLocation(): { page: PageId; nextPath: string } {
  if (typeof window === 'undefined') {
    return { page: 'overview', nextPath: '/' };
  }
  captureDesktopTokenFromUrl();
  const rawPath = window.location.pathname || '/';
  const redirected = LEGACY_REDIRECTS[rawPath];
  if (redirected) {
    window.history.replaceState(null, '', `${redirected}${window.location.search}`);
  }
  const page = pageForPath(redirected ?? rawPath);
  const params = new URLSearchParams(window.location.search);
  const nextPath = sanitizeNextPath(params.get('next')) ?? '/';
  return { page, nextPath };
}

function readInitialTheme(): ThemeMode {
  if (typeof window === 'undefined') return 'light';
  try {
    const saved = window.localStorage.getItem('herTheme');
    if (saved === 'dark' || saved === 'light') return saved;
  } catch {
    // Ignore storage read failure.
  }
  return 'light';
}

function defaultTabsMap(): Record<string, string> {
  const initial: Record<string, string> = {};
  for (const item of NAV_ITEMS) {
    if (item.tabs?.length) {
      initial[item.id] = item.tabs[0].id;
    }
  }
  return initial;
}

export const ConsoleProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [{ page: initialPage, nextPath: initialNext }] = useState(readInitialLocation);
  const [page, setPage] = useState<PageId>(initialPage);
  const [nextPathAfterAuth, setNextPathAfterAuth] = useState<string>(initialNext);
  const [activeTabs, setActiveTabs] = useState<Record<string, string>>(defaultTabsMap);
  const [theme, setTheme] = useState<ThemeMode>(readInitialTheme);
  const [navDrawerOpen, setNavDrawerOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [inspector, setInspectorState] = useState<InspectorTarget>(null);
  const inspectorRef = useRef<InspectorTarget>(null);
  const inspectorDirtyRef = useRef(false);
  const [highlightedKey, setHighlightedKey] = useState<string | null>(null);

  useEffect(() => {
    inspectorRef.current = inspector;
  }, [inspector]);
  const [authStatus, setAuthStatus] = useState<AuthStatusResponse | null>(null);
  const [authLoading, setAuthLoading] = useState(true);
  const [expiredNotice, setExpiredNotice] = useState<string | null>(null);
  const [runState, setRunState] = useState('检查中');
  const [globalMode, setGlobalMode] = useState('—');
  const [pendingCount, setPendingCount] = useState(0);
  const [updateAvailable, setUpdateAvailable] = useState<string | null>(null);
  const [qqLoginState, setQqLoginState] = useState('unknown');
  const [settings, setSettings] = useState<SettingItemView[]>([]);
  const [conversations, setConversations] = useState<ConversationStateSummary[]>([]);
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const [confirmRequest, setConfirmRequest] = useState<ConfirmRequest | null>(null);
  const [dirtyEntry, setDirtyEntry] = useState<DirtyEntry | null>(null);
  const dirtyEntryRef = useRef<DirtyEntry | null>(null);
  const pageRef = useRef<PageId>(initialPage);
  const activeTabsRef = useRef<Record<string, string>>({});
  const lastPathRef = useRef<string>(
    typeof window !== 'undefined' ? `${window.location.pathname}${window.location.search}` : '/',
  );
  const histIdxRef = useRef<number>(
    typeof window !== 'undefined' && typeof window.history.state?.herHistIdx === 'number'
      ? window.history.state.herHistIdx
      : 0,
  );
  const suppressPopRef = useRef(0);
  const bypassGuardOnceRef = useRef(false);
  const liveSeqRef = useRef(0);
  const qqEventSeqRef = useRef(0);

  useEffect(() => {
    pageRef.current = page;
  }, [page]);

  useEffect(() => {
    activeTabsRef.current = activeTabs;
  }, [activeTabs]);

  useEffect(() => {
    dirtyEntryRef.current = dirtyEntry;
  }, [dirtyEntry]);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === 'dark') {
      root.classList.add('dark');
    } else {
      root.classList.remove('dark');
    }
    try {
      window.localStorage.setItem('herTheme', theme);
    } catch {
      // Ignore storage write failure.
    }
  }, [theme]);

  const dismissToast = useCallback((id: string) => {
    setToasts((prev) => prev.filter((item) => item.id !== id));
  }, []);

  const pushToast = useCallback(
    (message: string, tone: ToastTone = 'info', undo?: { label: string; run: () => void }) => {
      const id = `${Date.now()}-${Math.random().toString(16).slice(2, 6)}`;
      setToasts((prev) => [
        ...prev.slice(-3),
        { id, message, tone, undoLabel: undo?.label, onUndo: undo?.run },
      ]);
      window.setTimeout(() => {
        setToasts((prev) => prev.filter((item) => item.id !== id));
      }, 4500);
    },
    [],
  );

  const applyNavigation = useCallback((fullPath: string, tabId?: string, replace = false) => {
    const url = new URL(fullPath, window.location.origin);
    const redirected = LEGACY_REDIRECTS[url.pathname];
    const targetPathname = redirected ?? url.pathname;
    const nextUrl = `${targetPathname}${url.search}`;
    if (`${window.location.pathname}${window.location.search}` !== nextUrl) {
      if (replace) {
        window.history.replaceState(
          { ...(window.history.state ?? {}), herHistIdx: histIdxRef.current },
          '',
          nextUrl,
        );
      } else {
        histIdxRef.current += 1;
        window.history.pushState({ herHistIdx: histIdxRef.current }, '', nextUrl);
      }
    }
    lastPathRef.current = nextUrl;
    const targetPage = pageForPath(targetPathname);
    setPage(targetPage);
    if (tabId) {
      activeTabsRef.current = { ...activeTabsRef.current, [targetPage]: tabId };
      setActiveTabs((prev) => ({ ...prev, [targetPage]: tabId }));
    }
    if (targetPage === 'login') {
      const nextParam = sanitizeNextPath(url.searchParams.get('next'));
      if (nextParam) setNextPathAfterAuth(nextParam);
    }
    setNavDrawerOpen(false);
  }, []);

  const navigateWithGuard = useCallback(
    (fullPath: string, tabId?: string) => {
      const url = new URL(fullPath, window.location.origin);
      const targetPathname = LEGACY_REDIRECTS[url.pathname] ?? url.pathname;
      const targetPage = pageForPath(targetPathname);
      const curDirty = dirtyEntryRef.current;
      const switchingTabWithDirty =
        targetPage === pageRef.current &&
        tabId !== undefined &&
        tabId !== activeTabsRef.current[targetPage];
      if (
        (targetPage !== pageRef.current || switchingTabWithDirty) &&
        targetPage !== 'login' &&
        targetPage !== 'setup' &&
        curDirty &&
        curDirty.count > 0
      ) {
        setConfirmRequest({
          title: '丢弃未保存的更改？',
          description: `${curDirty.label || '当前页面存在未保存更改'}，离开或切换标签页将会丢失这些修改。`,
          confirmLabel: '放弃并离开',
          danger: true,
          onConfirm: () => {
            curDirty.onDiscard();
            dirtyEntryRef.current = null;
            setDirtyEntry(null);
            applyNavigation(fullPath, tabId);
          },
        });
        return;
      }
      applyNavigation(fullPath, tabId);
    },
    [applyNavigation],
  );

  const navigatePath = useCallback(
    (fullPath: string) => {
      navigateWithGuard(fullPath);
    },
    [navigateWithGuard],
  );

  const navigate = useCallback(
    (targetPage: PageId, tabId?: string, query?: string) => {
      const basePath = pathForPage(targetPage);
      const nextUrl = query ? `${basePath}?${query}` : basePath;
      navigateWithGuard(nextUrl, tabId);
    },
    [navigateWithGuard],
  );

  const setActiveTab = useCallback((pageId: string, tabId: string) => {
    activeTabsRef.current = { ...activeTabsRef.current, [pageId]: tabId };
    setActiveTabs((prev) => ({ ...prev, [pageId]: tabId }));
  }, []);

  const refreshLiveData = useCallback(async () => {
    const seq = ++liveSeqRef.current;
    const qqEpoch = qqEventSeqRef.current;
    const [statusRes, pendingRes, settingsRes, statesRes, qqRes, releasesRes] =
      await Promise.allSettled([
        fetchConsoleStatus(),
        fetchPendingItems(),
        fetchSettingsList(),
        fetchConversationStates(),
        fetchQqLoginState(),
        fetchReleaseStatus(),
      ]);

    if (seq !== liveSeqRef.current) return;

    let fallbackUpdate: string | null = null;
    if (statusRes.status === 'fulfilled') {
      const s = statusRes.value;
      const stopped = s.socialV2Paused === true || s.stopped === true || s.paused === true;
      setRunState(stopped ? '已暂停' : '运行中');
      if (typeof s.mode === 'string' && s.mode) {
        setGlobalMode(s.mode);
      }
      if (typeof s.updateAvailable === 'string' && s.updateAvailable) {
        fallbackUpdate = s.updateAvailable;
      }
    }

    if (releasesRes.status === 'fulfilled') {
      const tag = releasesRes.value.available?.tag;
      setUpdateAvailable(typeof tag === 'string' && tag ? tag : fallbackUpdate);
    } else if (statusRes.status === 'fulfilled') {
      setUpdateAvailable(fallbackUpdate);
    }

    if (pendingRes.status === 'fulfilled') {
      const items = Array.isArray(pendingRes.value.pending)
        ? pendingRes.value.pending
        : pendingRes.value.items;
      setPendingCount(Array.isArray(items) ? items.length : 0);
    }

    if (settingsRes.status === 'fulfilled') {
      setSettings(Array.isArray(settingsRes.value.settings) ? settingsRes.value.settings : []);
    }

    if (statesRes.status === 'fulfilled') {
      const raw = statesRes.value;
      const listRaw = Array.isArray(raw.conversations)
        ? raw.conversations
        : Array.isArray(raw.states)
          ? raw.states
          : [];
      const mapped: ConversationStateSummary[] = listRaw
        .filter((item): item is Record<string, unknown> => Boolean(item && typeof item === 'object'))
        .map((item) => ({
          key: String(item.key ?? item.conversation ?? ''),
          name: typeof item.name === 'string' ? item.name : undefined,
          phase: typeof item.phase === 'string' ? item.phase : undefined,
          unread:
            typeof item.unreadCount === 'number'
              ? item.unreadCount
              : typeof item.unread === 'number'
                ? item.unread
                : undefined,
          nextWakeAt: typeof item.nextWakeAt === 'number' ? item.nextWakeAt : undefined,
        }))
        .filter((item) => item.key.length > 0);
      setConversations(mapped);
    }

    if (
      qqEpoch === qqEventSeqRef.current &&
      qqRes.status === 'fulfilled' &&
      typeof qqRes.value.state === 'string'
    ) {
      setQqLoginState(qqRes.value.state);
    }
  }, []);

  const refreshAuthAndShell = useCallback(async () => {
    setAuthLoading(true);
    try {
      const status = await fetchAuthStatus();
      setAuthStatus(status);
      if (!status.passwordSet && !isDesktopOrigin()) {
        if (pageRef.current !== 'setup') {
          navigate('setup');
        }
        return;
      }
      if (!status.authenticated) {
        if (pageRef.current !== 'login' && pageRef.current !== 'setup') {
          const current = `${window.location.pathname}${window.location.search}`;
          const safe = sanitizeNextPath(current) ?? '/';
          setNextPathAfterAuth(safe);
          navigate('login', undefined, `next=${encodeURIComponent(safe)}`);
        }
        return;
      }
      if (pageRef.current === 'login' || pageRef.current === 'setup') {
        const dest = sanitizeNextPath(new URLSearchParams(window.location.search).get('next')) ?? '/';
        navigatePath(dest);
      }
      await refreshLiveData();
    } catch {
      // Initial auth check may fail if legacy fixture mocks only partial endpoints.
    } finally {
      setAuthLoading(false);
    }
  }, [navigate, navigatePath, refreshLiveData]);

  useEffect(() => {
    setUnauthorizedHandler(() => {
      const current = `${window.location.pathname}${window.location.search}`;
      const safe = sanitizeNextPath(current) ?? '/';
      setNextPathAfterAuth(safe);
      setExpiredNotice('会话已过期，请重新登录');
      navigate('login', undefined, `next=${encodeURIComponent(safe)}`);
    });
    return () => setUnauthorizedHandler(null);
  }, [navigate]);

  useEffect(() => {
    void refreshAuthAndShell();
  }, [refreshAuthAndShell]);

  useEffect(() => {
    if (typeof window !== 'undefined' && typeof window.history.state?.herHistIdx !== 'number') {
      window.history.replaceState(
        { ...(window.history.state ?? {}), herHistIdx: histIdxRef.current },
        '',
        `${window.location.pathname}${window.location.search}`,
      );
    }

    const onPopState = () => {
      if (suppressPopRef.current > 0) {
        suppressPopRef.current -= 1;
        return;
      }

      setConfirmRequest(null);

      const raw = window.location.pathname || '/';
      const redirected = LEGACY_REDIRECTS[raw];
      const resolvedPath = `${redirected ?? raw}${window.location.search}`;
      const targetPage = pageForPath(redirected ?? raw);
      const targetIdx =
        typeof window.history.state?.herHistIdx === 'number'
          ? window.history.state.herHistIdx
          : null;

      if (bypassGuardOnceRef.current) {
        bypassGuardOnceRef.current = false;
        if (targetIdx !== null) {
          histIdxRef.current = targetIdx;
        }
        if (redirected) {
          window.history.replaceState(
            { ...(window.history.state ?? {}), herHistIdx: histIdxRef.current },
            '',
            resolvedPath,
          );
        }
        lastPathRef.current = resolvedPath;
        setPage(targetPage);
        return;
      }

      const curDirty = dirtyEntryRef.current;
      const hasPageDirty = Boolean(curDirty && curDirty.count > 0);
      const hasDrawerDirty =
        (targetPage === 'login' || targetPage === 'setup') && inspectorDirtyRef.current;

      if (targetPage !== pageRef.current && (hasPageDirty || hasDrawerDirty)) {
        const currentIdx = histIdxRef.current;
        const deltaToCurrent =
          targetIdx !== null && targetIdx !== currentIdx ? currentIdx - targetIdx : 1;

        suppressPopRef.current += 1;
        window.history.go(deltaToCurrent);

        const label = hasPageDirty
          ? curDirty?.label || '当前页面存在未保存更改'
          : '右侧抽屉中有未保存的设置修改';

        setConfirmRequest({
          title: '丢弃未保存的更改？',
          description: `${label}，浏览器后退/前进将会丢失这些修改。`,
          confirmLabel: '放弃并离开',
          danger: true,
          onConfirm: () => {
            if (histIdxRef.current !== currentIdx) return;
            if (hasPageDirty && curDirty) {
              curDirty.onDiscard();
              dirtyEntryRef.current = null;
              setDirtyEntry(null);
            }
            if (hasDrawerDirty) {
              inspectorDirtyRef.current = false;
              setInspectorState(null);
            }
            bypassGuardOnceRef.current = true;
            window.history.go(-deltaToCurrent);
          },
        });
        return;
      }

      if (targetIdx !== null) {
        histIdxRef.current = targetIdx;
      }
      if (redirected) {
        window.history.replaceState(
          { ...(window.history.state ?? {}), herHistIdx: histIdxRef.current },
          '',
          resolvedPath,
        );
      }
      lastPathRef.current = resolvedPath;
      setPage(targetPage);
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  useEffect(() => {
    const fallbackBanner = document.getElementById('her-qq-login');
    if (fallbackBanner) {
      fallbackBanner.remove();
    }
    window.herQqLogin = (state: string) => {
      const preHydration = document.getElementById('her-qq-login');
      if (preHydration) {
        preHydration.remove();
      }
      qqEventSeqRef.current += 1;
      setQqLoginState(state || 'unknown');
    };
  }, []);

  const openConfirm = useCallback((req: ConfirmRequest) => {
    setConfirmRequest(req);
  }, []);

  const closeConfirm = useCallback(() => {
    setConfirmRequest(null);
  }, []);

  const setInspectorDirty = useCallback((dirty: boolean) => {
    inspectorDirtyRef.current = dirty;
  }, []);

  const setInspector = useCallback((target: InspectorTarget) => {
    const cur = inspectorRef.current;
    const sameTarget =
      cur === target ||
      (cur !== null &&
        target !== null &&
        cur.kind === target.kind &&
        (cur.kind === 'setting'
          ? cur.key === (target as { kind: 'setting'; key: string }).key
          : cur.id === (target as { id: string }).id));
    if (sameTarget) {
      return;
    }
    if (cur !== null && inspectorDirtyRef.current) {
      openConfirm({
        title: '丢弃未保存的检视修改？',
        description: '当前右侧抽屉中存在尚未保存的修改，切换或关闭将会丢失这些修改。',
        confirmLabel: '丢弃并继续',
        danger: true,
        onConfirm: () => {
          inspectorDirtyRef.current = false;
          setInspectorState(target);
        },
      });
      return;
    }
    inspectorDirtyRef.current = false;
    setInspectorState(target);
  }, [openConfirm]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
        if (pageRef.current === 'login' || pageRef.current === 'setup') {
          return;
        }
        event.preventDefault();
        setPaletteOpen((prev) => !prev);
      } else if (event.key === 'Escape') {
        if (paletteOpen) {
          setPaletteOpen(false);
        } else if (confirmRequest) {
          setConfirmRequest(null);
        } else if (inspector) {
          setInspector(null);
        } else if (navDrawerOpen) {
          setNavDrawerOpen(false);
        }
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [confirmRequest, inspector, navDrawerOpen, paletteOpen, setInspector]);

  const toggleTheme = useCallback(() => {
    setTheme((prev) => (prev === 'light' ? 'dark' : 'light'));
  }, []);

  const clearExpiredNotice = useCallback(() => {
    setExpiredNotice(null);
  }, []);

  const signOut = useCallback(async () => {
    try {
      await submitLogout();
    } catch {
      // Still transition to /login so local view drops session state.
    }
    setAuthStatus((prev) => (prev ? { ...prev, authenticated: false, authKind: null } : prev));
    inspectorDirtyRef.current = false;
    setInspectorState(null);
    pushToast('已退出控制台会话', 'info');
    navigate('login');
  }, [navigate, pushToast]);

  const triggerQqLoginWindow = useCallback(() => {
    if (window.ipc) {
      window.ipc.postMessage('qq-login');
      pushToast('已请求打开 QQ 登录窗口', 'info');
    } else {
      pushToast('请在宿主设备或桌面应用中完成 QQ 登录', 'warn');
    }
  }, [pushToast]);

  const jumpToSetting = useCallback(
    (key: string, openDrawer = true) => {
      setPaletteOpen(false);
      navigate('settings', 'registry');
      setHighlightedKey(key);
      if (openDrawer) {
        setInspector({ kind: 'setting', key });
      }
      window.setTimeout(() => {
        setHighlightedKey((curr) => (curr === key ? null : curr));
      }, 2400);
    },
    [navigate, setInspector],
  );

  const saveSetting = useCallback(
    async (setting: string, value: unknown, confirm = false) => {
      const res = await saveSettingValue(setting, value, confirm);
      await refreshLiveData();
      const mirrorErr = typeof res.mirrorError === 'string' && res.mirrorError ? res.mirrorError : null;
      const wakeErr =
        typeof res.wakeRefreshError === 'string' && res.wakeRefreshError
          ? res.wakeRefreshError
          : null;
      const warn = [mirrorErr, wakeErr].filter(Boolean).join('；');
      if (warn) {
        pushToast(`已保存设置 ${setting}（写回警告：${warn}）`, 'warn');
      } else {
        pushToast(`已保存设置 ${setting}`, 'success');
      }
    },
    [pushToast, refreshLiveData],
  );

  const resetSetting = useCallback(
    async (setting: string, confirm = false) => {
      const res = await resetSettingValue(setting, confirm);
      await refreshLiveData();
      const mirrorErr = typeof res.mirrorError === 'string' && res.mirrorError ? res.mirrorError : null;
      const wakeErr =
        typeof res.wakeRefreshError === 'string' && res.wakeRefreshError
          ? res.wakeRefreshError
          : null;
      const warn = [mirrorErr, wakeErr].filter(Boolean).join('；');
      if (warn) {
        pushToast(`已恢复默认设置 ${setting}（写回警告：${warn}）`, 'warn');
      } else {
        pushToast(`已恢复默认设置 ${setting}`, 'success');
      }
    },
    [pushToast, refreshLiveData],
  );

  return (
    <ConsoleContext.Provider
      value={{
        page,
        activeTabs,
        setActiveTab,
        navigate,
        navigatePath,
        nextPathAfterAuth,
        authStatus,
        authLoading,
        expiredNotice,
        clearExpiredNotice,
        refreshAuthAndShell,
        refreshLiveData,
        signOut,
        theme,
        toggleTheme,
        navDrawerOpen,
        setNavDrawerOpen,
        paletteOpen,
        setPaletteOpen,
        inspector,
        setInspector,
        setInspectorDirty,
        highlightedKey,
        jumpToSetting,
        runState,
        globalMode,
        pendingCount,
        updateAvailable,
        qqLoginState,
        triggerQqLoginWindow,
        settings,
        conversations,
        saveSetting,
        resetSetting,
        toasts,
        pushToast,
        dismissToast,
        confirmRequest,
        openConfirm,
        closeConfirm,
        dirtyEntry,
        setDirtyEntry,
      }}
    >
      {children}
    </ConsoleContext.Provider>
  );
};

export function useConsole(): ConsoleContextValue {
  const ctx = useContext(ConsoleContext);
  if (!ctx) {
    throw new Error('useConsole must be used inside ConsoleProvider');
  }
  return ctx;
}
