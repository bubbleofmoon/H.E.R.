/// <reference types="vite/client" />

declare global {
  interface Window {
    herQqLogin?: (state: string) => void;
    ipc?: {
      postMessage: (message: string) => void;
    };
  }
}

export {};
