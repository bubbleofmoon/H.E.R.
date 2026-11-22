import React from 'react';
import { CommandPalette } from './components/CommandPalette';
import { ConfirmModal, StickySaveBar, ToastHost } from './components/Primitives';
import { WorkbenchLayout } from './layouts/WorkbenchLayout';
import { ConversationsPage } from './pages/ConversationsPage';
import { LegacyPage } from './pages/LegacyPage';
import { LoginPage } from './pages/LoginPage';
import { OverviewPage } from './pages/OverviewPage';
import { SetupPage } from './pages/SetupPage';
import { WorkspacePlaceholderPage } from './pages/WorkspacePlaceholderPage';
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
        ) : page === 'legacy' ? (
          <LegacyPage />
        ) : (
          <WorkspacePlaceholderPage />
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
