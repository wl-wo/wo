/**
 * Wo Electron Preload Script
 */

import { contextBridge, ipcRenderer } from 'electron';
import type {
  CompositorAPI,
  WoClientAPI,
  WindowMetadataEvent,
  FocusChangeEvent,
  WoWindow,
  DamageFramePayload,
  RendererDamageHelper,
  RendererDamageHelperOptions,
} from 'wo-types';
import { createRendererDamageHelper } from './renderer-damage-helper.js';

const compositorAPI: CompositorAPI = {
  action: async (name: string, payload?: Record<string, unknown>) => {
    return await ipcRenderer.invoke('action', name, payload ?? {});
  },
  actionSync: (name: string, payload?: Record<string, unknown>) => {
    ipcRenderer.send('action-sync', name, payload ?? {});
  },
  quit: (code?: number) => ipcRenderer.invoke('action', 'quit', { code: code ?? 0 }),
  reload: () => ipcRenderer.invoke('action', 'reload', {}),
  toggleDevTools: () => ipcRenderer.invoke('action', 'devtools', {}),
  
  // New: Focus change notifications
  onFocusChange: (callback: (window: string, focused: boolean) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: FocusChangeEvent) => 
      callback(data.window, data.focused);
    ipcRenderer.on('wo:focus-change', handler);
    return () => ipcRenderer.removeListener('wo:focus-change', handler);
  },
  
  // New: Window metadata
  onWindowMetadata: (callback: (metadata: WindowMetadataEvent) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, metadata: WindowMetadataEvent) => callback(metadata);
    ipcRenderer.on('wo:window-metadata', handler);
    return () => ipcRenderer.removeListener('wo:window-metadata', handler);
  },
  
  // New: Syscall API
  syscall: async (type: string, params: Record<string, unknown>) => {
    return await ipcRenderer.invoke('wo:syscall', type, params);
  },

  submitDamageFrame: async (payload: DamageFramePayload) => {
    return await ipcRenderer.invoke('wo:submit-damage-frame', payload);
  },

  // Surface buffer: Wayland window pixel content from compositor
  // Legacy path for backward compatibility (full pixel copy via IPC)
  onSurfaceBuffer: (callback: (data: { name: string; width: number; height: number; stride: number; pixels: Buffer; damageRects?: Array<{x: number; y: number; width: number; height: number}> }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: { name: string; width: number; height: number; stride: number; pixels: Buffer; damageRects?: Array<{x: number; y: number; width: number; height: number}> }) =>
      callback(data);
    ipcRenderer.on('wo:surface-buffer', handler);
    return () => ipcRenderer.removeListener('wo:surface-buffer', handler);
  },

  // Zero-copy surface buffer via SharedArrayBuffer.
  // 'wo:surface-sab' delivers the SAB once per window (or on resize).
  // 'wo:surface-update' is a lightweight signal (no pixels) when new data is ready.
  onSurfaceSab: (callback: (data: { name: string; sab: SharedArrayBuffer; width: number; height: number; stride: number }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: { name: string; sab: SharedArrayBuffer; width: number; height: number; stride: number }) =>
      callback(data);
    ipcRenderer.on('wo:surface-sab', handler);
    return () => ipcRenderer.removeListener('wo:surface-sab', handler);
  },

  onSurfaceUpdate: (callback: (data: { name: string; width: number; height: number; stride: number; generation: number; damageRects?: Array<{x: number; y: number; width: number; height: number}> }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: { name: string; width: number; height: number; stride: number; generation: number; damageRects?: Array<{x: number; y: number; width: number; height: number}> }) =>
      callback(data);
    ipcRenderer.on('wo:surface-update', handler);
    return () => ipcRenderer.removeListener('wo:surface-update', handler);
  },

  createDamageHelper: (options: RendererDamageHelperOptions): RendererDamageHelper => {
    return createRendererDamageHelper(
      async (payload: DamageFramePayload) => ipcRenderer.invoke('wo:submit-damage-frame', payload),
      options,
    );
  },

  // Notification events from compositor
  onNotification: (callback: (data: { id?: string; title: string; body: string; icon?: string; timeout?: number; timestamp?: number }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: { id?: string; title: string; body: string; icon?: string; timeout?: number; timestamp?: number }) =>
      callback(data);
    ipcRenderer.on('wo:notification', handler);
    return () => ipcRenderer.removeListener('wo:notification', handler);
  },

  // Forward keyboard event from canvas to compositor (evdev keycode)
  forwardKeyboard: (windowName: string, evdevKey: number, pressed: boolean, time: number) => {
    ipcRenderer.send('wo:forward-keyboard', windowName, evdevKey, pressed, time);
  },

  // Forward relative mouse motion
  forwardRelativePointer: (windowName: string, dx: number, dy: number) => {
    ipcRenderer.send('wo:forward-relative-pointer', windowName, dx, dy);
  },

  // Pointer lock request from compositor
  onPointerLockRequest: (callback: (data: { window: string; lock: boolean }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: { window: string; lock: boolean }) =>
      callback(data);
    ipcRenderer.on('wo:pointer-lock-request', handler);
    return () => ipcRenderer.removeListener('wo:pointer-lock-request', handler);
  },

  // Environment variable updates from compositor (e.g. DISPLAY after XWayland ready)
  onEnvUpdate: (callback: (vars: Record<string, string>) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, vars: Record<string, string>) =>
      callback(vars);
    ipcRenderer.on('wo:env-update', handler);
    return () => ipcRenderer.removeListener('wo:env-update', handler);
  },

  // Generic portal request event for client-provided popup/approval handlers
  onPortalRequest: (callback: (data: {
    requestId: string;
    kind: string;
    appName?: string;
    sessionId?: string;
  }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: {
      requestId: string;
      kind: string;
      appName?: string;
      sessionId?: string;
    }) => callback(data);
    ipcRenderer.on('wo:portal-request', handler);
    return () => ipcRenderer.removeListener('wo:portal-request', handler);
  },

  // Screencopy capture event notifications
  onScreencopyEvent: (callback: (data: { active: boolean; clientCount: number }) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, data: { active: boolean; clientCount: number }) =>
      callback(data);
    ipcRenderer.on('wo:screencopy-event', handler);
    return () => ipcRenderer.removeListener('wo:screencopy-event', handler);
  },
};

const woClientAPI: WoClientAPI = {
  getWindows: async () => ipcRenderer.invoke('wo:get-windows') as Promise<WoWindow[]>,
  onWindows: (callback: (windows: WoWindow[]) => void) => {
    const handler = (_event: Electron.IpcRendererEvent, windows: WoWindow[]) => callback(windows);
    ipcRenderer.on('wo:windows', handler);
    return () => ipcRenderer.removeListener('wo:windows', handler);
  },
  sendAction: (action: string) => {
    ipcRenderer.send('wo:keybind-action', action);
  },
};

contextBridge.exposeInMainWorld('compositor', compositorAPI);
contextBridge.exposeInMainWorld('woClient', woClientAPI);

export type { CompositorAPI, WoClientAPI };
