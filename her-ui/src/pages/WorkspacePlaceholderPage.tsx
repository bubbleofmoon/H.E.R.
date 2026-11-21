import React from 'react';
import { NAV_ITEMS } from '../catalog/navigation';
import { Badge, Button, Card, CardHeader, PageHeader, Tabs } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';

export const WorkspacePlaceholderPage: React.FC = () => {
  const {
    page,
    activeTabs,
    setActiveTab,
    settings,
    conversations,
    highlightedKey,
    setInspector,
    refreshLiveData,
  } = useConsole();

  const nav = NAV_ITEMS.find((item) => item.id === page) ?? NAV_ITEMS[0];
  const currentTab = activeTabs[nav.id] ?? nav.tabs?.[0]?.id ?? '';

  return (
    <div className="space-y-4">
      <PageHeader
        title={nav.label}
        description={nav.description}
        badge={<Badge tone="accent">{nav.sliceTicket}</Badge>}
        actions={
          <Button size="sm" variant="secondary" onClick={() => void refreshLiveData()}>
            刷新数据
          </Button>
        }
      />

      {nav.tabs && nav.tabs.length > 0 && (
        <Tabs
          tabs={nav.tabs}
          active={currentTab}
          onChange={(tabId) => setActiveTab(nav.id, tabId)}
        />
      )}

      {nav.id === 'settings' && (
        <Card>
          <CardHeader
            title="实时设置索引"
            subtitle={`已加载 ${settings.length} 项注册设置（点击任意项可在右侧抽屉中直接编辑或重置）`}
          />
          <div className="divide-y divide-[var(--border-hairline)]">
            {settings.slice(0, 24).map((item) => (
              <button
                key={item.key}
                type="button"
                onClick={() => setInspector({ kind: 'setting', key: item.key })}
                className={`flex w-full items-center justify-between gap-3 px-4 py-2.5 text-left text-xs hover:bg-[var(--bg-subtle)] ${
                  highlightedKey === item.key ? 'row-flash' : ''
                }`}
              >
                <div className="min-w-0">
                  <div className="font-medium text-[var(--text-primary)]">{item.label}</div>
                  <div className="font-mono text-[11px] text-[var(--text-muted)]">{item.key}</div>
                </div>
                <Badge tone={item.confirm ? 'warn' : 'neutral'}>{item.applies}</Badge>
              </button>
            ))}
          </div>
        </Card>
      )}

      {nav.id !== 'settings' && (
        <Card>
          <CardHeader
            title={`${nav.label}工作台基础框架已就绪`}
            subtitle={`当前已连接 ${conversations.length} 个会话与 ${settings.length} 项实时设置，具体业务模块将在 ${nav.sliceTicket} 接入。`}
          />
          <div className="p-4 text-xs text-[var(--text-secondary)]">
            可使用顶部检索框或 <kbd className="rounded bg-[var(--bg-subtle)] px-1 font-mono">Ctrl+K</kbd>{' '}
            打开全局命令面板，快速切换页面、搜索设置键名并在右侧检视抽屉中编辑。
          </div>
        </Card>
      )}
    </div>
  );
};
