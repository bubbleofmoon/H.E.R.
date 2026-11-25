import React from 'react';
import { PageHeader, Tabs } from '../components/Primitives';
import { useGuardedTab } from '../utils/herHelpers';
import { BehaviorParamsTab } from './behavior/BehaviorParamsTab';
import { DiagnosticsTab } from './behavior/DiagnosticsTab';

const BEHAVIOR_TABS = [
  { id: 'params', label: '模式与参数' },
  { id: 'diagnostics', label: '诊断日志' },
] as const;

const VALID_TABS = ['params', 'diagnostics'] as const;

export const BehaviorPage: React.FC = () => {
  const [currentTab, handleTabChange] = useGuardedTab('behavior', 'params', VALID_TABS);

  return (
    <div className="space-y-4">
      <PageHeader
        title="参与模式与参数"
        description="集中管理运行模式、角色参与度、唤醒与回复节奏、工具开关及运行诊断日志。"
      />

      <Tabs
        tabs={BEHAVIOR_TABS.map((t) => ({ id: t.id, label: t.label }))}
        active={currentTab}
        onChange={handleTabChange}
      />

      {currentTab === 'params' ? <BehaviorParamsTab /> : <DiagnosticsTab />}
    </div>
  );
};
