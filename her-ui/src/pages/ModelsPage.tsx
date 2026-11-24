import React from 'react';
import { PageHeader, Tabs } from '../components/Primitives';
import { useGuardedTab } from '../utils/herHelpers';
import { ModelRoutesTab } from './models/ModelRoutesTab';
import { SpendTab } from './models/SpendTab';

const MODEL_TABS = [
  { id: 'routes', label: '模型路由' },
  { id: 'spend', label: '用量与花费' },
] as const;

const VALID_TABS = ['routes', 'spend'] as const;

export const ModelsPage: React.FC = () => {
  const [currentTab, handleTabChange] = useGuardedTab('models', 'routes', VALID_TABS);

  return (
    <div className="space-y-4">
      <PageHeader
        title="模型与用量"
        description="管理全局与按会话模型路由、模型参数、端点凭据与各维度 Token 花费账本。"
      />

      <Tabs
        tabs={MODEL_TABS.map((t) => ({ id: t.id, label: t.label }))}
        active={currentTab}
        onChange={handleTabChange}
      />

      {currentTab === 'routes' ? <ModelRoutesTab /> : <SpendTab />}
    </div>
  );
};
