import React, { useEffect, useMemo, useRef, useState } from 'react';
import {
  CornerDownLeft,
  MessageSquare,
  Moon,
  RefreshCw,
  Search,
  SlidersHorizontal,
  Sparkles,
  Sun,
  X,
} from 'lucide-react';
import { NAV_ITEMS } from '../catalog/navigation';
import { useConsole } from '../stores/consoleStore';

interface PaletteItem {
  id: string;
  category: '页面与标签页' | '实时设置项' | '会话' | '常用快捷操作';
  title: string;
  subtitle: string;
  searchBlob: string;
  onSelect: () => void;
}

const KEY_PINYIN_ALIASES: Record<string, string> = {
  reply: 'hf hq huanxing jingmo',
  wake: 'hx hxpl zdhx',
  send: 'fs dxfs fy',
  voice: 'yy yylb yyfs',
  sticker: 'bqb',
  media: 'mt',
  memory: 'jy',
  model: 'mx',
  harness: 'mxly ly',
  security: 'aq',
  backup: 'bf hy',
  allow: 'bmd yx',
  deny: 'hmd jj',
  owner: 'gl yz',
  prompt: 'tsc ms',
  schedule: 'rc',
};

function settingSearchBlob(key: string, label: string, group: string): string {
  const aliases = Object.entries(KEY_PINYIN_ALIASES)
    .filter(([token]) => key.toLowerCase().includes(token))
    .map(([, p]) => p)
    .join(' ');
  return `${label} ${key} ${group} ${aliases}`.toLowerCase();
}

export const CommandPalette: React.FC = () => {
  const {
    paletteOpen,
    setPaletteOpen,
    navigate,
    settings,
    jumpToSetting,
    conversations,
    setInspector,
    theme,
    toggleTheme,
    refreshLiveData,
    pushToast,
  } = useConsole();

  const [query, setQuery] = useState('');
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (paletteOpen) {
      setQuery('');
      setActiveIndex(0);
      window.setTimeout(() => inputRef.current?.focus(), 20);
    }
  }, [paletteOpen]);

  const allItems = useMemo<PaletteItem[]>(() => {
    const pages: PaletteItem[] = [];
    for (const nav of NAV_ITEMS) {
      pages.push({
        id: `nav-${nav.id}`,
        category: '页面与标签页',
        title: nav.label,
        subtitle: `${nav.groupLabel} · ${nav.path}`,
        searchBlob: `${nav.label} ${nav.shortLabel} ${nav.pinyin} ${nav.path} ${nav.description}`.toLowerCase(),
        onSelect: () => navigate(nav.id),
      });
      for (const tab of nav.tabs ?? []) {
        pages.push({
          id: `tab-${nav.id}-${tab.id}`,
          category: '页面与标签页',
          title: `${nav.label} › ${tab.label}`,
          subtitle: `${nav.groupLabel} · 标签页`,
          searchBlob: `${nav.label} ${tab.label} ${nav.pinyin} ${tab.pinyin}`.toLowerCase(),
          onSelect: () => navigate(nav.id, tab.id),
        });
      }
    }

    const settingItems: PaletteItem[] = settings.map((s) => ({
      id: `set-${s.key}`,
      category: '实时设置项',
      title: s.label,
      subtitle: `${s.group} · ${s.key}`,
      searchBlob: settingSearchBlob(s.key, s.label, s.group),
      onSelect: () => jumpToSetting(s.key, true),
    }));

    const convItems: PaletteItem[] = conversations.map((c) => ({
      id: `conv-${c.key}`,
      category: '会话',
      title: c.name || c.key,
      subtitle: `${c.key}${c.phase ? ` · ${c.phase}` : ''}`,
      searchBlob: `${c.name ?? ''} ${c.key} ${c.phase ?? ''} hh`.toLowerCase(),
      onSelect: () => {
        navigate('conversations');
        setInspector({ kind: 'conversation', id: c.key });
      },
    }));

    const quickItems: PaletteItem[] = [
      {
        id: 'act-theme',
        category: '常用快捷操作',
        title: theme === 'light' ? '切换到深色模式' : '切换到浅色模式',
        subtitle: '界面显示偏好',
        searchBlob: '深色 浅色 主题 theme dark light ssms',
        onSelect: toggleTheme,
      },
      {
        id: 'act-refresh',
        category: '常用快捷操作',
        title: '立即刷新运行状态',
        subtitle: '拉取最新心跳、待处理队列与设置列表',
        searchBlob: '刷新 状态 refresh sx',
        onSelect: () => {
          void refreshLiveData();
          pushToast('已刷新运行状态', 'info');
        },
      },
    ];

    return [...pages, ...convItems, ...settingItems, ...quickItems];
  }, [
    conversations,
    jumpToSetting,
    navigate,
    pushToast,
    refreshLiveData,
    setInspector,
    settings,
    theme,
    toggleTheme,
  ]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) {
      return allItems.slice(0, 14);
    }
    const tokens = q.split(/\s+/);
    return allItems.filter((item) => tokens.every((t) => item.searchBlob.includes(t))).slice(0, 18);
  }, [allItems, query]);

  useEffect(() => {
    setActiveIndex(0);
  }, [query]);

  if (!paletteOpen) return null;

  const categories: PaletteItem['category'][] = ['页面与标签页', '会话', '实时设置项', '常用快捷操作'];
  const grouped = categories
    .map((cat) => ({
      category: cat,
      items: filtered.filter((item) => item.category === cat),
    }))
    .filter((group) => group.items.length > 0);

  const runItem = (item: PaletteItem) => {
    setPaletteOpen(false);
    item.onSelect();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/45 px-4 pt-[12vh]"
      role="dialog"
      aria-modal="true"
      aria-label="全局命令搜索"
      onClick={(e) => {
        if (e.target === e.currentTarget) setPaletteOpen(false);
      }}
    >
      <div className="w-full max-w-xl overflow-hidden rounded-xl border border-[var(--border-strong)] bg-[var(--bg-surface)] shadow-2xl">
        <div className="flex items-center gap-2.5 border-b border-[var(--border-hairline)] px-3.5 py-2.5">
          <Search className="h-4 w-4 shrink-0 text-[var(--text-muted)]" />
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                setActiveIndex((prev) => (filtered.length ? (prev + 1) % filtered.length : 0));
              } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                setActiveIndex((prev) =>
                  filtered.length ? (prev - 1 + filtered.length) % filtered.length : 0,
                );
              } else if (e.key === 'Enter' && filtered[activeIndex]) {
                e.preventDefault();
                runItem(filtered[activeIndex]);
              }
            }}
            placeholder="搜索页面、拼音缩写（如 hh / bf）、设置键名（如 sendVoice）或会话…"
            className="h-7 flex-1 bg-transparent text-[13.5px] text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:outline-none"
          />
          <button
            type="button"
            onClick={() => setPaletteOpen(false)}
            aria-label="关闭命令面板"
            className="rounded p-1 text-[var(--text-muted)] hover:bg-[var(--bg-hover)]"
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <div className="max-h-[400px] overflow-y-auto p-2">
          {grouped.length === 0 ? (
            <div className="py-10 text-center text-xs text-[var(--text-muted)]">
              没有找到匹配的页面、设置项或会话。试试输入 <code>hh</code>、<code>bf</code> 或{' '}
              <code>sendVoice</code>。
            </div>
          ) : (
            grouped.map((g) => (
              <div key={g.category} className="mb-2 last:mb-0">
                <div className="px-2.5 py-1 text-[11px] font-semibold text-[var(--text-muted)]">
                  {g.category}
                </div>
                {g.items.map((item) => {
                  const flatIdx = filtered.indexOf(item);
                  const isActive = flatIdx === activeIndex;
                  return (
                    <button
                      key={item.id}
                      type="button"
                      onMouseEnter={() => setActiveIndex(flatIdx)}
                      onClick={() => runItem(item)}
                      className={`flex w-full items-center justify-between gap-3 rounded-lg px-2.5 py-2 text-left transition-colors ${
                        isActive ? 'bg-[var(--accent-soft)] text-[var(--accent)]' : 'hover:bg-[var(--bg-subtle)]'
                      }`}
                    >
                      <div className="flex min-w-0 items-center gap-2.5">
                        <span className="text-[var(--text-muted)]">
                          {item.category === '会话' ? (
                            <MessageSquare className="h-3.5 w-3.5" />
                          ) : item.category === '实时设置项' ? (
                            <SlidersHorizontal className="h-3.5 w-3.5" />
                          ) : item.category === '常用快捷操作' ? (
                            item.id === 'act-theme' ? (
                              theme === 'light' ? (
                                <Moon className="h-3.5 w-3.5" />
                              ) : (
                                <Sun className="h-3.5 w-3.5" />
                              )
                            ) : (
                              <RefreshCw className="h-3.5 w-3.5" />
                            )
                          ) : (
                            <Sparkles className="h-3.5 w-3.5" />
                          )}
                        </span>
                        <div className="min-w-0">
                          <div className="truncate text-[13px] font-medium text-[var(--text-primary)]">
                            {item.title}
                          </div>
                          <div className="truncate font-mono text-[11px] text-[var(--text-muted)]">
                            {item.subtitle}
                          </div>
                        </div>
                      </div>
                      {isActive && <CornerDownLeft className="h-3.5 w-3.5 shrink-0 opacity-70" />}
                    </button>
                  );
                })}
              </div>
            ))
          )}
        </div>

        <div className="flex items-center justify-between border-t border-[var(--border-hairline)] bg-[var(--bg-subtle)]/70 px-3.5 py-1.5 text-[11px] text-[var(--text-muted)]">
          <span>↑↓ 选择 · Enter 跳转高亮 · Esc 关闭</span>
          <span>支持中文、拼音首字母、完整键名</span>
        </div>
      </div>
    </div>
  );
};
