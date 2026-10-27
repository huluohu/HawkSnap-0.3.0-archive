/**
 * Minimal hand-written typings for the chrome.* APIs this extension uses.
 *
 * Zero-runtime-dependency rule (plan ledger Ruling 3) means no `@types/chrome`
 * devDependency — this file declares only the surface we touch. esbuild does
 * not typecheck, so these types exist to document intent and help editors.
 */

declare namespace chrome {
  namespace action {
    const onClicked: {
      addListener(cb: (tab: tabs.Tab) => void): void;
    };
    function setBadgeText(details: { text: string; tabId?: number }): void;
    function setBadgeBackgroundColor(details: { color: string; tabId?: number }): void;
  }

  namespace commands {
    const onCommand: {
      addListener(cb: (command: string) => void): void;
    };
  }

  namespace runtime {
    const id: string;
    function getURL(path: string): string;
    function getManifest<T = Record<string, unknown>>(): T;
    function sendMessage(message: unknown): Promise<unknown>;
    function sendMessage(message: unknown, callback: (response: unknown) => void): void;
    const onMessage: {
      addListener(
        cb: (
          message: unknown,
          sender: { tab?: { id?: number; windowId?: number }; frameId?: number },
          sendResponse: (response?: unknown) => boolean | void,
        ) => boolean | void,
      ): void;
    };
    interface Port {
      name: string;
      postMessage(message: unknown): void;
      disconnect(): void;
      onMessage: { addListener(cb: (message: unknown) => void): void };
      onDisconnect: { addListener(cb: () => void): void };
    }
    function connect(extensionIdOrName?: string): Port;
    function connectNative(hostName: string): Port;
    function getContexts(filter: {
      contextType: 'TAB' | 'SERVICE_WORKER' | 'OFFSCREEN_DOCUMENT';
    }): Promise<unknown[]>;
    let lastError: { message: string } | undefined;
  }

  namespace scripting {
    function executeScript(injection: {
      target: { tabId: number };
      files?: string[];
      func?: (...args: unknown[]) => unknown;
      args?: unknown[];
    }): Promise<Array<{ result?: unknown }>>;
    function insertCSS(injection: {
      target: { tabId: number };
      css: string;
    }): Promise<void>;
  }

  namespace tabs {
    interface Tab {
      id?: number;
      windowId?: number;
      url?: string;
      title?: string;
    }
    function create(createProperties: { url: string; active?: boolean }): Promise<Tab>;
    function query(queryInfo: {
      active?: boolean;
      currentWindow?: boolean;
      lastFocusedWindow?: boolean;
    }): Promise<Tab[]>;
    function captureVisibleTab(
      windowId?: number,
      options?: { format?: 'jpeg' | 'png'; quality?: number },
    ): Promise<string>;
    function sendMessage(tabId: number, message: unknown): Promise<unknown>;
  }

  namespace downloads {
    function download(options: {
      url: string;
      filename?: string;
      saveAs?: boolean;
    }): Promise<number>;
  }

  namespace storage {
    namespace session {
      function get(keys?: string | string[] | null): Promise<Record<string, unknown>>;
      function set(items: Record<string, unknown>): Promise<void>;
      function remove(keys: string | string[]): Promise<void>;
    }
  }

  namespace offscreen {
    interface CreateParameters {
      url: string;
      reasons: string[];
      justification: string;
    }
    function createDocument(params: CreateParameters): Promise<void>;
    function closeDocument(path?: string): Promise<void>;
    function hasDocument(): Promise<boolean>;
  }
}

// `debugger` 是保留字，不能作 namespace/const 名（TS2819/TS1389）——
// 用点分环境命名空间（与上面的 declare namespace chrome 声明合并），
// 用法 chrome.debugger.* 不变。
declare namespace chrome.debugger {
  interface Debuggee {
    tabId: number;
  }
  function attach(target: Debuggee, requiredVersion: string): Promise<void>;
  function detach(target: Debuggee): Promise<void>;
  function sendCommand(
    target: Debuggee,
    method: string,
    params?: Record<string, unknown>,
  ): Promise<Record<string, unknown>>;
  const onDetach: {
    addListener(cb: (source: Debuggee, reason: string) => void): void;
  };
}
