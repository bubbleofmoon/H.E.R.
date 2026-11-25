import React, { useEffect, useState } from 'react';
import {
  Archive,
  ArrowUpCircle,
  Bell,
  ChevronDown,
  ChevronRight,
  LogOut,
  Menu,
  Moon,
  PanelRightOpen,
  Search,
  Sun,
  X,
} from 'lucide-react';
import { NAV_GROUPS, NAV_ITEMS } from '../catalog/navigation';
import { useConsole } from '../stores/consoleStore';
import { NavIcon } from '../components/NavIcons';
import { InspectorDrawer } from '../components/InspectorDrawer';
import { Badge, QqLoginBar } from '../components/Primitives';

export const WorkbenchLayout: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const {
    page,
    navigate,
    navDrawerOpen,
    setNavDrawerOpen,
    setPaletteOpen,
    runState,
    globalMode,
    pendingCount,
    updateAvailable,
    theme,
    toggleTheme,
    inspector,
    setInspector,
    conversations,
    signOut,
  } = useConsole();

  const [showLegacy, setShowLegacy] = useState(page === 'legacy');
  useEffect(() => {
    if (page === 'legacy') {
      setShowLegacy(true);
    }
  }, [page]);
  const firstConversationId = conversations[0]?.key ?? 'private:owner';

  const sidebarContent = (
    <div className="flex h-full flex-col bg-[var(--bg-surface)]">
      <div className="flex h-10 shrink-0 items-center justify-between border-b border-[var(--border-hairline)] px-3">
        <span className="text-[13px] font-bold tracking-tight text-[var(--text-primary)]">
          HER · 工作台
        </span>
        <button
          type="button"
          onClick={() => setNavDrawerOpen(false)}
          className="rounded p-1 lg:hidden"
          aria-label="关闭菜单"
        >
          <X className="h-4 w-4" />
        </button>
      </div>

      <nav className="flex-1 space-y-2 overflow-y-auto px-2 py-2" aria-label="主导航">
        {NAV_GROUPS.map((group) => {
          const items = NAV_ITEMS.filter((n) => n.groupId === group.id);
          return (
            <div key={group.id}>
              <div className="px-2 pb-0.5 text-[10px] font-semibold tracking-wider text-[var(--text-muted)]">
                {group.label}
              </div>
              {items.map((item) => {
                const active = page === item.id;
                return (
                  <button
                    key={item.id}
                    type="button"
                    onClick={() => navigate(item.id)}
                    className={`flex w-full items-center gap-2 rounded px-2 py-1.5 text-[12px] transition-colors ${
                      active
                        ? 'bg-[var(--accent-soft)] font-semibold text-[var(--accent)]'
                        : 'text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]'
                    }`}
                  >
                    <NavIcon id={item.id} className="h-3.5 w-3.5 shrink-0" />
                    <span className="truncate">{item.shortLabel}</span>
                  </button>
                );
              })}
            </div>
          );
        })}
      </nav>

      <div className="border-t border-[var(--border-hairline)] p-2">
        <button
          type="button"
          aria-expanded={showLegacy}
          onClick={() => setShowLegacy((v) => !v)}
          className={`flex w-full items-center justify-between rounded px-2 py-1 text-[11.5px] ${
            page === 'legacy'
              ? 'font-semibold text-[var(--accent)]'
              : 'text-[var(--text-muted)] hover:bg-[var(--bg-hover)]'
          }`}
        >
          <span className="flex items-center gap-2">
            <Archive className="h-3.5 w-3.5 shrink-0" />
            <span>旧版</span>
          </span>
          {showLegacy ? (
            <ChevronDown className="h-3.5 w-3.5 shrink-0" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 shrink-0" />
          )}
        </button>
        {showLegacy && (
          <div className="mt-1 pl-2">
            <button
              type="button"
              onClick={() => navigate('legacy')}
              className={`flex w-full items-center justify-between rounded px-2 py-1.5 text-[11.5px] transition-colors ${
                page === 'legacy'
                  ? 'bg-[var(--accent-soft)] font-semibold text-[var(--accent)]'
                  : 'text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]'
              }`}
            >
              <span className="truncate">旧版工具隔离区</span>
              <span className="text-[10px] text-[var(--warn)]">#78</span>
            </button>
          </div>
        )}
      </div>
    </div>
  );

  return (
    <div className="flex h-screen w-full overflow-hidden bg-[var(--bg-canvas)]">
      <aside className="hidden w-[184px] shrink-0 border-r border-[var(--border-hairline)] lg:block">
        {sidebarContent}
      </aside>

      {navDrawerOpen && (
        <div className="fixed inset-0 z-50 flex lg:hidden">
          <div className="w-[210px] border-r border-[var(--border-hairline)] shadow-2xl">
            {sidebarContent}
          </div>
          <div className="flex-1 bg-black/40" onClick={() => setNavDrawerOpen(false)} />
        </div>
      )}

      <div className="flex min-w-0 flex-1 flex-col">
        <QqLoginBar />

        <header className="flex h-10 shrink-0 items-center justify-between gap-2 border-b border-[var(--border-hairline)] bg-[var(--bg-surface)] px-3.5">
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => setNavDrawerOpen(true)}
              className="inline-flex h-7 w-7 items-center justify-center rounded border border-[var(--border-hairline)] lg:hidden"
              aria-label="打开菜单"
            >
              <Menu className="h-4 w-4" />
            </button>
            <button
              type="button"
              onClick={() => setPaletteOpen(true)}
              className="flex h-7 w-72 max-w-[46vw] items-center justify-between gap-2 rounded border border-[var(--border-strong)] bg-[var(--bg-subtle)]/70 px-2.5 text-xs text-[var(--text-muted)] hover:border-[var(--accent)]"
            >
              <span className="flex items-center gap-1.5 truncate">
                <Search className="h-3.5 w-3.5 shrink-0" />
                <span className="truncate">命令与实时检索 (Ctrl+K)…</span>
              </span>
              <kbd className="rounded bg-[var(--bg-surface)] px-1 font-mono text-[10px]">Ctrl+K</kbd>
            </button>
          </div>

          <div className="flex items-center gap-1.5">
            <Badge tone={runState === '运行中' ? 'success' : 'warn'}>{runState}</Badge>
            <Badge tone="neutral" className="hidden sm:inline-flex">
              {globalMode}
            </Badge>
            {pendingCount > 0 && (
              <button
                type="button"
                onClick={() => navigate('overview')}
                className="inline-flex h-7 items-center gap-1 rounded border border-amber-300 bg-[var(--warn-soft)] px-2 text-xs font-medium text-[var(--warn)]"
              >
                <Bell className="h-3 w-3" />
                <span>{pendingCount}</span>
              </button>
            )}
            {updateAvailable && (
              <button
                type="button"
                onClick={() => navigate('system', 'updates')}
                className="inline-flex h-7 items-center gap-1 rounded border border-blue-300 bg-[var(--accent-soft)] px-2 text-xs font-medium text-[var(--accent)]"
              >
                <ArrowUpCircle className="h-3 w-3" />
                <span className="hidden md:inline">有更新</span>
              </button>
            )}
            <button
              type="button"
              onClick={() =>
                setInspector(inspector ? null : { kind: 'conversation', id: firstConversationId })
              }
              title="打开/关闭右侧详情抽屉"
              className={`inline-flex h-7 items-center gap-1 rounded border px-2 text-xs ${
                inspector
                  ? 'border-[var(--accent)] bg-[var(--accent-soft)] text-[var(--accent)]'
                  : 'border-[var(--border-strong)] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]'
              }`}
            >
              <PanelRightOpen className="h-3.5 w-3.5" />
              <span className="hidden md:inline">检视抽屉</span>
            </button>
            <button
              type="button"
              onClick={toggleTheme}
              aria-label={theme === 'light' ? '切换到深色模式' : '切换到浅色模式'}
              className="inline-flex h-7 w-7 items-center justify-center rounded text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]"
            >
              {theme === 'light' ? (
                <Moon className="h-3.5 w-3.5" />
              ) : (
                <Sun className="h-3.5 w-3.5" />
              )}
            </button>
            <button
              type="button"
              onClick={() => void signOut()}
              aria-label="退出登录"
              className="inline-flex h-7 w-7 items-center justify-center rounded text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]"
            >
              <LogOut className="h-3.5 w-3.5" />
            </button>
          </div>
        </header>

        <main className="flex-1 overflow-y-auto px-4 py-4 pb-20 sm:px-6">
          <div className="mx-auto max-w-6xl">{children}</div>
        </main>
      </div>

      <InspectorDrawer />
    </div>
  );
};
