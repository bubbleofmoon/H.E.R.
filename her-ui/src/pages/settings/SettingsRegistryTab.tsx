import React, { useEffect, useMemo, useRef, useState } from 'react';
import { SlidersHorizontal } from 'lucide-react';
import { SettingItemView } from '../../api/client';
import { EmptyState, SearchInput } from '../../components/ConsoleWidgets';
import { Badge, Button, Card, SettingField, Switch } from '../../components/Primitives';
import { useConsole } from '../../stores/consoleStore';
import { formatAppliesLabel } from '../../utils/format';
import {
  INPUT_CLASS,
  localizeSettingItem,
  settingValuesEqual,
  useSettingsDrafts,
} from '../../utils/herHelpers';

const GROUP_LABELS: Record<string, string> = {
  'reply rhythm': '回复节奏',
  wake: '唤醒与等待',
  'send limits': '发送限制与拟真',
  'model and cost': '模型与成本',
  'prompt text': '人设与提示词文本',
  security: '安全与准入（需确认）',
  'turn engine': '回合引擎',
  'memory and digest': '记忆与摘要',
  schedule: '日程调度',
  'endpoint credentials': '外部端点凭据',
  'console authentication': '控制台鉴权',
  tools: '工具开关',
  voice: '语音合成',
  stickers: '表情包',
  media: '媒体与图片',
  alerts: '告警与通知',
};

function groupLabel(group: string): string {
  return GROUP_LABELS[group] ? `${GROUP_LABELS[group]} (${group})` : group;
}

function isSettingNonDefault(item: SettingItemView): boolean {
  if (item.kind.type === 'password') {
    return Boolean(item.isSet);
  }
  return !settingValuesEqual(item.value, item.default);
}

export const SettingsRegistryTab: React.FC = () => {
  const { settings, highlightedKey, setInspector } = useConsole();

  const [search, setSearch] = useState('');
  const [selectedGroup, setSelectedGroup] = useState<string>('all');
  const [nonDefaultOnly, setNonDefaultOnly] = useState(false);

  const rowRefs = useRef<Record<string, HTMLDivElement | null>>({});

  const localizedSettings = useMemo(() => settings.map(localizeSettingItem), [settings]);

  const editableSettings = useMemo(
    () => localizedSettings.filter((item) => item.managedBy !== 'consoleAuth'),
    [localizedSettings],
  );

  const { drafts, setDraft, handleReset } = useSettingsDrafts(editableSettings, '全局设置');

  const nonDefaultCount = useMemo(
    () => localizedSettings.filter(isSettingNonDefault).length,
    [localizedSettings],
  );

  const groupCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const item of localizedSettings) {
      counts.set(item.group, (counts.get(item.group) ?? 0) + 1);
    }
    return Array.from(counts.entries());
  }, [localizedSettings]);

  useEffect(() => {
    if (!highlightedKey) return;
    setSearch('');
    setSelectedGroup('all');
    setNonDefaultOnly(false);
    const timer = window.setTimeout(() => {
      const target = rowRefs.current[highlightedKey];
      if (target) {
        target.scrollIntoView({ behavior: 'smooth', block: 'center' });
      }
    }, 60);
    return () => window.clearTimeout(timer);
  }, [highlightedKey]);

  const filteredSettings = useMemo(() => {
    const q = search.trim().toLowerCase();
    return localizedSettings.filter((item) => {
      if (selectedGroup !== 'all' && item.group !== selectedGroup) return false;
      if (nonDefaultOnly && !isSettingNonDefault(item)) return false;
      if (!q) return true;
      return (
        item.key.toLowerCase().includes(q) ||
        item.label.toLowerCase().includes(q) ||
        item.hint.toLowerCase().includes(q) ||
        item.group.toLowerCase().includes(q)
      );
    });
  }, [localizedSettings, selectedGroup, nonDefaultOnly, search]);

  const groupedSections = useMemo(() => {
    const map = new Map<string, SettingItemView[]>();
    for (const item of filteredSettings) {
      const list = map.get(item.group);
      if (list) {
        list.push(item);
      } else {
        map.set(item.group, [item]);
      }
    }
    return Array.from(map.entries());
  }, [filteredSettings]);

  const clearFilters = () => {
    setSearch('');
    setSelectedGroup('all');
    setNonDefaultOnly(false);
  };

  return (
    <div className="space-y-4">
      <Card className="p-4">
        <div className="flex flex-wrap items-center gap-3">
          <div className="min-w-[240px] flex-1">
            <SearchInput
              value={search}
              onChange={setSearch}
              placeholder="搜索设置键名、标签、说明或分组…"
            />
          </div>

          <select
            aria-label="按设置分组筛选"
            value={selectedGroup}
            onChange={(e) => setSelectedGroup(e.target.value)}
            className={`${INPUT_CLASS} w-auto min-w-[220px]`}
          >
            <option value="all">全部分组（{localizedSettings.length} 项）</option>
            {groupCounts.map(([group, count]) => (
              <option key={group} value={group}>
                {groupLabel(group)} · {count}
              </option>
            ))}
          </select>

          <div className="flex items-center gap-2 text-xs text-[var(--text-secondary)]">
            <Switch
              checked={nonDefaultOnly}
              onChange={setNonDefaultOnly}
              label="仅看非默认值"
            />
            <span>仅看非默认 ({nonDefaultCount})</span>
          </div>

          {(search || selectedGroup !== 'all' || nonDefaultOnly) && (
            <Button size="sm" variant="ghost" onClick={clearFilters}>
              清空筛选
            </Button>
          )}
        </div>
      </Card>

      {groupedSections.length === 0 ? (
        <EmptyState
          icon={<SlidersHorizontal className="h-5 w-5" />}
          title="没有匹配的设置项"
          description="尝试清空搜索条件或切换「仅看非默认值」筛选。"
          action={
            <Button size="sm" variant="secondary" onClick={clearFilters}>
              显示全部设置
            </Button>
          }
        />
      ) : (
        groupedSections.map(([group, items]) => (
          <Card key={group} className="p-4">
            <div className="mb-3 flex flex-wrap items-center justify-between gap-2 border-b border-[var(--border-hairline)] pb-2">
              <h2 className="text-[14px] font-semibold text-[var(--text-primary)]">
                {groupLabel(group)}
              </h2>
              <Badge tone={group === 'security' ? 'warn' : 'neutral'}>{items.length} 项</Badge>
            </div>

            <div className="grid gap-3 md:grid-cols-2">
              {items.map((item) => {
                const isModified = isSettingNonDefault(item);
                const isHighlighted = highlightedKey === item.key;
                const readOnly = item.managedBy === 'consoleAuth';
                return (
                  <div
                    key={item.key}
                    ref={(el) => {
                      rowRefs.current[item.key] = el;
                    }}
                    data-setting-key={item.key}
                    className={`rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/35 p-3 ${
                      isHighlighted ? 'row-flash ring-2 ring-[var(--accent)]' : ''
                    }`}
                  >
                    <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <Badge tone={item.confirm ? 'warn' : 'neutral'}>
                          {formatAppliesLabel(item.applies)}
                        </Badge>
                        {item.confirm && <Badge tone="warn">需确认</Badge>}
                        {isModified && <Badge tone="accent">非默认</Badge>}
                        {item.overriddenBy?.source && (
                          <Badge tone="warn">覆盖: {item.overriddenBy.source}</Badge>
                        )}
                        {readOnly && <Badge tone="neutral">只读（控制台鉴权托管）</Badge>}
                      </div>
                      <button
                        type="button"
                        onClick={() => setInspector({ kind: 'setting', key: item.key })}
                        className="text-[11px] font-medium text-[var(--accent)] hover:underline"
                      >
                        详情抽屉
                      </button>
                    </div>

                    <SettingField
                      setting={item}
                      draftValue={drafts[item.key]}
                      onDraftChange={
                        readOnly ? undefined : (next) => setDraft(item.key, next)
                      }
                      onReset={readOnly ? undefined : () => handleReset(item.key)}
                    />
                  </div>
                );
              })}
            </div>
          </Card>
        ))
      )}
    </div>
  );
};
