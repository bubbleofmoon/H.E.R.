import React from 'react';
import { CommandPalette } from './components/CommandPalette';
import { ConfirmModal, StickySaveBar, ToastHost } from './components/Primitives';
import { WorkbenchLayout } from './layouts/WorkbenchLayout';
import { AuditPage } from './pages/AuditPage';
import { BehaviorPage } from './pages/BehaviorPage';
import { ConversationsPage } from './pages/ConversationsPage';
import { LegacyPage } from './pages/LegacyPage';
import { LoginPage } from './pages/LoginPage';
import { MemoryPage } from './pages/MemoryPage';
import { ModelsPage } from './pages/ModelsPage';
import { OverviewPage } from './pages/OverviewPage';
import { PersonaPage } from './pages/PersonaPage';
import { SchedulePage } from './pages/SchedulePage';
import { SecurityPage } from './pages/SecurityPage';
import { SettingsPage } from './pages/SettingsPage';
import { SetupPage } from './pages/SetupPage';
import { SystemPage } from './pages/SystemPage';
import { ConsoleProvider, useConsole } from './stores/consoleStore';

const ConsoleRouter: React.FC = () => {
  const { page } = useConsole();

  if (page === 'login') {
    return (
      <>
        <LoginPage />
        <ToastHost />
      </>
    );
  }

  if (page === 'setup') {
    return (
      <>
        <SetupPage />
        <ToastHost />
      </>
    );
  }

  return (
    <>
      <WorkbenchLayout>
        {page === 'overview' ? (
          <OverviewPage />
        ) : page === 'conversations' ? (
          <ConversationsPage />
        ) : page === 'persona' ? (
          <PersonaPage />
        ) : page === 'memory' ? (
          <MemoryPage />
        ) : page === 'behavior' ? (
          <BehaviorPage />
        ) : page === 'models' ? (
          <ModelsPage />
        ) : page === 'schedule' ? (
          <SchedulePage />
        ) : page === 'system' ? (
          <SystemPage />
        ) : page === 'security' ? (
          <SecurityPage />
        ) : page === 'settings' ? (
          <SettingsPage />
        ) : page === 'audit' ? (
          <AuditPage />
        ) : page === 'legacy' ? (
          <LegacyPage />
        ) : (
          <OverviewPage />
        )}
      </WorkbenchLayout>
      <StickySaveBar />
      <CommandPalette />
      <ConfirmModal />
      <ToastHost />
    </>
  );
};

export const App: React.FC = () => (
  <ConsoleProvider>
    <ConsoleRouter />
  </ConsoleProvider>
);

export default App;
