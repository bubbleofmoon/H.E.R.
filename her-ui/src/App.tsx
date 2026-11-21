import React from 'react';
import { CommandPalette } from './components/CommandPalette';
import { ConfirmModal, StickySaveBar, ToastHost } from './components/Primitives';
import { WorkbenchLayout } from './layouts/WorkbenchLayout';
import { LegacyPage } from './pages/LegacyPage';
import { LoginPage } from './pages/LoginPage';
import { SchedulePage } from './pages/SchedulePage';
import { SetupPage } from './pages/SetupPage';
import { SystemPage } from './pages/SystemPage';
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
        {page === 'schedule' ? (
          <SchedulePage />
        ) : page === 'system' ? (
          <SystemPage />
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
