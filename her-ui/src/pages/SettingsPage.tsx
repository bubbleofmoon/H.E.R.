import React from 'react';
import { PageHeader, Tabs } from '../components/Primitives';
import { useGuardedTab } from '../utils/herHelpers';
import { SettingsHistoryTab } from './settings/SettingsHistoryTab';
import { SettingsRegistryTab } from './settings/SettingsRegistryTab';

const SETTINGS_TABS = [
  { id: 'registry', label: '设置索引' },
  { id: 'history', label: '变更历史' },
] as const;

const VALID_TABS = ['registry', 'history'] as const;

export const SettingsPage: React.FC = () => {
  const [currentTab, handleTabChange] = useGuardedTab('settings', 'registry', VALID_TABS);

  return (
    <div className="space-y-4">
      <PageHeader
        title="全部设置与变更历史"
        description="按分组检索全部运行时设置项、识别非默认与覆盖状态，并审计或撤销历史配置变更。"
      />

      <Tabs
        tabs={SETTINGS_TABS.map((t) => ({ id: t.id, label: t.label }))}
        active={currentTab}
        onChange={handleTabChange}
      />

      {currentTab === 'registry' ? <SettingsRegistryTab /> : <SettingsHistoryTab />}
    </div>
  );
};
