import React from 'react';
import { PageHeader, Tabs } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';
import { MediaVoiceTab } from './memory/MediaVoiceTab';
import { MemoryLibraryTab } from './memory/MemoryLibraryTab';
import { SlangTab } from './memory/SlangTab';

const MEMORY_TABS = [
  { id: 'library', label: '记忆库' },
  { id: 'slang', label: '黑话词库' },
  { id: 'media', label: '表情包与语音' },
];

export const MemoryPage: React.FC = () => {
  const { activeTabs, setActiveTab, dirtyEntry, openConfirm } = useConsole();
  const rawTab = activeTabs['memory'];
  const currentTab = rawTab === 'slang' || rawTab === 'media' ? rawTab : 'library';

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
          setActiveTab('memory', nextTab);
        },
      });
      return;
    }
    setActiveTab('memory', nextTab);
  };

  return (
    <div className="space-y-4">
      <PageHeader
        title="记忆与知识"
        description="检索与维护长期记忆、会话轻记忆、黑话词库、QQ 表情包与语音素材库。"
      />

      <Tabs tabs={MEMORY_TABS} active={currentTab} onChange={handleTabChange} />

      {currentTab === 'library' ? (
        <MemoryLibraryTab />
      ) : currentTab === 'slang' ? (
        <SlangTab />
      ) : (
        <MediaVoiceTab />
      )}
    </div>
  );
};
