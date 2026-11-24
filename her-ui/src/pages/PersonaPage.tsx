import React from 'react';
import { PageHeader, Tabs } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';
import { PersonaLibraryTab } from './persona/PersonaLibraryTab';
import { SimPromptTab } from './persona/SimPromptTab';

const PERSONA_TABS = [
  { id: 'personas', label: '人设库' },
  { id: 'prompts', label: '仿真提示词' },
];

export const PersonaPage: React.FC = () => {
  const { activeTabs, setActiveTab, dirtyEntry, openConfirm } = useConsole();
  const currentTab = activeTabs['persona'] === 'prompts' ? 'prompts' : 'personas';

  const handleTabChange = (nextTab: string) => {
    if (nextTab === currentTab) return;
    if (dirtyEntry && dirtyEntry.count > 0) {
      openConfirm({
        title: '丢弃未保存的更改？',
        description: `${dirtyEntry.label || '当前标签页有未保存的更改'}，切换标签页将会丢失这些修改。`,
        confirmLabel: '放弃并切换',
        danger: true,
        onConfirm: () => {
          dirtyEntry.onDiscard();
          setActiveTab('persona', nextTab);
        },
      });
      return;
    }
    setActiveTab('persona', nextTab);
  };

  return (
    <div className="space-y-4">
      <PageHeader
        title="人设与提示词"
        description="统一管理版本化人设库、唤醒词与撤销记录、仿真提示词预设及规则文本设置。"
      />

      <Tabs tabs={PERSONA_TABS} active={currentTab} onChange={handleTabChange} />

      {currentTab === 'personas' ? <PersonaLibraryTab /> : <SimPromptTab />}
    </div>
  );
};
