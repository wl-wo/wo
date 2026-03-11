/**
 * Window types and interfaces from the Wayland compositor
 */

/**
 * Represents a single window managed by the compositor
 */
export interface WoWindow {
  /** Unique identifier for this window */
  name: string;

  /** Window title/label */
  title?: string;

  /** X coordinate of window in pixels */
  x: number;

  /** Y coordinate of window in pixels */
  y: number;

  /** Width of window in pixels */
  width: number;

  /** Height of window in pixels */
  height: number;

  /** Z-order (stacking) of the window (higher = on top) */
  z_order?: number;

  /** Whether this window is currently mapped/visible */
  mapped?: boolean;

  /** Whether this window is currently focused */
  focused?: boolean;

  /** Frames per second being rendered */
  fps?: number;

  /** Process ID of the application (if available) */
  pid?: number;

  /** Window source: 'electron', 'wayland', or 'x11' (XWayland clients). */
  source?: 'electron' | 'wayland' | 'x11';

  /** True when server-side decorations/chrome should be drawn by comraw. */
  ssd?: boolean;

  /** Application ID (for Wayland windows) */
  app_id?: string;

  /** True if this is a dialog window (xdg_toplevel with set_parent) */
  dialog?: boolean;

  /** Name of the parent window for dialog windows (e.g. "wayland-0") */
  parent_name?: string;
}

/**
 * Action types for compositor window control
 */
export type CompositorActionType =
  | 'focus' | 'minimize' | 'close' | 'resize' | 'move' | 'maximize'
  | 'pointer_motion' | 'pointer_button' | 'pointer_leave' | 'pointer_scroll' | 'keyboard_key';

/**
 * Configuration for compositor actions
 */
export interface CompositorActionConfig {
  window?: string;
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  [key: string]: unknown;
}

// Stronger syscall typing

export interface SyscallListApplicationsParams { [key: string]: any; }
export interface SyscallLaunchParams { command: string; [key: string]: any; }
export interface SyscallExecParams { command: string; [key: string]: any; }
export interface SyscallListDirParams { path?: string; [key: string]: any; }
export interface SyscallReadParams { path: string; [key: string]: any; }
export interface SyscallWriteParams { path: string; content: string; [key: string]: any; }
export interface SyscallShutdownParams { [key: string]: any; }
export interface SyscallRestartParams { [key: string]: any; }
export interface SyscallLogoutParams { [key: string]: any; }
export interface SyscallLockParams { [key: string]: any; }
export interface SyscallSleepParams { [key: string]: any; }
export interface SyscallNotifyParams { id?: string; title: string; body?: string; icon?: string; timeout?: number; [key: string]: any; }
export interface SyscallPortalRespondParams { requestId: string; response?: any; allowed?: boolean; type?: string; windowName?: string | null; [key: string]: any; }
export interface SyscallDbusCallParams { service: string; objectPath: string; interface: string; method: string; signature?: string; args?: any[]; bus?: 'user' | 'system'; [key: string]: any; }
export interface SyscallDbusGetPropertyParams { service: string; objectPath: string; interface: string; property: string; bus?: 'user' | 'system'; [key: string]: any; }
export interface SyscallLoadLocalFileParams { path: string; [key: string]: any; }

export interface SyscallLaunchResult { ok: boolean; pid?: number; error?: string; }
export interface SyscallExecResult { ok: boolean; pid?: number; error?: string; }
export interface SyscallListDirResult { ok: boolean; path?: string; entries?: { name: string; type: 'file'|'dir' }[]; error?: string; }
export interface SyscallReadResult { ok: boolean; content?: string; size?: number; error?: string; }
export interface SyscallWriteResult { ok: boolean; bytesWritten?: number; error?: string; }
export interface SyscallShutdownResult { ok: boolean; error?: string; }
export interface SyscallRestartResult { ok: boolean; error?: string; }
export interface SyscallLogoutResult { ok: boolean; error?: string; }
export interface SyscallLockResult { ok: boolean; error?: string; }
export interface SyscallSleepResult { ok: boolean; error?: string; }
export interface SyscallNotifyResult { ok: boolean; error?: string; }
export interface SyscallPortalRespondResult { ok: boolean; error?: string; }
export interface SyscallDbusCallResult { ok: boolean; stdout?: string; stderr?: string; exitCode?: number; error?: string; }
export interface SyscallDbusGetPropertyResult { ok: boolean; stdout?: string; stderr?: string; exitCode?: number; error?: string; }
export interface SyscallLoadLocalFileResult { ok: boolean; error?: string; }

export interface PortalRequestEvent {
  requestId: string;
  payload: any;
  kind?: string;
}

export interface ScreencopyEvent {
  active: boolean;
  clientCount: number;
}

/**
 * Application information for launching
 */
export interface ApplicationInfo {
  name: string;
  command: string;
  multi_instance?: boolean;
  icon?: ApplicationIcon;
}

/**
 * Icon data for applications - supports multiple formats
 */
export interface ApplicationIcon {
  /** Type of icon: 'iconify', 'base64', 'url', or 'path' */
  type: 'iconify' | 'base64' | 'url' | 'path';
  /** Icon data or reference */
  data: string;
  /** Optional MIME type (e.g., 'image/png', 'image/svg+xml') */
  mimeType?: string;
  /** Optional fallback icon if primary fails */
  fallback?: ApplicationIcon;
}

/**
 * Focus change callback signature
 */
export type FocusChangeCallback = (windowName: string, focused: boolean) => void;

/**
 * Window list update callback signature
 */
export type WindowsCallback = (windows: WoWindow[]) => void;

/**
 * Pixel buffer update callback signature
 */
export type PixelBufferCallback = (windowName: string, buffer: Uint8Array, width: number, height: number) => void;

/**
 * Unsubscribe function type
 */
export type Unsubscribe = () => void;

/**
 * Wayland client interface for receiving window updates
 */
export interface WoClient {
  /**
   * Get the current window list once
   */
  getWindows(): Promise<WoWindow[]>;

  /**
   * Subscribe to window list updates
   */
  onWindows(callback: WindowsCallback): Unsubscribe;

  /**
   * Subscribe to pixel buffer updates (DMABUF transfers, if supported)
   */
  onPixelBuffer?: (callback: PixelBufferCallback) => Unsubscribe;

  /**
   * Send a keybind/management action to the compositor
   */
  sendAction(action: string): void;
}

/** Alias for WoClient to match preload.ts imports */
export type WoClientAPI = WoClient;

/**
 * Compositor interface for controlling windows and the compositor
 */
export interface Compositor {
  /**
   * Send an action to modify a window
   */
  action(type: CompositorActionType, config: CompositorActionConfig): void;

  /**
   * Send an action synchronously
   */
  actionSync(type: string, payload?: Record<string, unknown>): void;

  /**
   * Execute a syscall (filesystem, process, etc.) with strong typings
   */
  syscall(type: 'list_applications', params?: SyscallListApplicationsParams): Promise<ApplicationInfo[]>;
  syscall(type: 'launch', params: SyscallLaunchParams): Promise<SyscallLaunchResult>;
  syscall(type: 'exec', params: SyscallExecParams): Promise<SyscallExecResult>;
  syscall(type: 'listdir', params?: SyscallListDirParams): Promise<SyscallListDirResult>;
  syscall(type: 'read', params: SyscallReadParams): Promise<SyscallReadResult>;
  syscall(type: 'write', params: SyscallWriteParams): Promise<SyscallWriteResult>;
  syscall(type: 'shutdown', params?: SyscallShutdownParams): Promise<SyscallShutdownResult>;
  syscall(type: 'restart', params?: SyscallRestartParams): Promise<SyscallRestartResult>;
  syscall(type: 'logout', params?: SyscallLogoutParams): Promise<SyscallLogoutResult>;
  syscall(type: 'lock', params?: SyscallLockParams): Promise<SyscallLockResult>;
  syscall(type: 'sleep', params?: SyscallSleepParams): Promise<SyscallSleepResult>;
  syscall(type: 'notify', params: SyscallNotifyParams): Promise<SyscallNotifyResult>;
  syscall(type: 'portal_respond', params: SyscallPortalRespondParams): Promise<SyscallPortalRespondResult>;
  syscall(type: 'dbus_call', params: SyscallDbusCallParams): Promise<SyscallDbusCallResult>;
  syscall(type: 'dbus_get_property', params: SyscallDbusGetPropertyParams): Promise<SyscallDbusGetPropertyResult>;
  syscall(type: 'load_local_file', params: SyscallLoadLocalFileParams): Promise<SyscallLoadLocalFileResult>;
  syscall(type: string, params?: any): Promise<any>;

  /**
   * Subscribe to focus changes
   */
  onFocusChange(callback: FocusChangeCallback): Unsubscribe;

  /**
   * Subscribe to raw window metadata broadcasts from the compositor
   */
  onWindowMetadata(callback: (metadata: WindowMetadataEvent) => void): Unsubscribe;

  /**
   * Create a damage helper for optimized rendering
   */
  createDamageHelper(options: RendererDamageHelperOptions): RendererDamageHelper;

  /**
   * Forward a keyboard event from a canvas window to the compositor.
   * evdevKey is the Linux evdev keycode; time is milliseconds.
   */
  forwardKeyboard(windowName: string, evdevKey: number, pressed: boolean, time: number): void;

  /**
   * Quit the compositor
   */
  quit(): void;

  /**
   * Reload the UI
   */
  reload(): void;

  /**
   * Toggle Electron DevTools
   */
  toggleDevTools(): void;

  /**
   * Submit a damaged frame from the renderer
   */
  submitDamageFrame(payload: DamageFramePayload): Promise<{ ok: boolean }>;

  /**
   * Subscribe to Wayland surface pixel buffer updates from the compositor.
   * The compositor captures each Wayland window's content offscreen and sends
   * raw ARGB8888 pixel data for the client to render.
   */
  onSurfaceBuffer(callback: (data: SurfaceBufferData) => void): Unsubscribe;

  /**
   * Subscribe to notification events from the compositor
   */
  onNotification(callback: (data: NotificationData) => void): Unsubscribe;


  /**
   * Forward relative mouse motion from the web UI to the compositor.
   */
  forwardRelativePointer(windowName: string, dx: number, dy: number): void;

  /**
   * Subscribe to pointer lock requests from the compositor.
   */
  onPointerLockRequest(callback: (data: { window: string; lock: boolean }) => void): Unsubscribe;

  /**
   * Subscribe to environment variable updates from the compositor
   * (e.g. DISPLAY becoming available after XWayland starts).
   */
  onEnvUpdate(callback: (vars: Record<string, string>) => void): Unsubscribe;

  /**
   * Subscribe to generic portal requests so clients can provide custom UI.
   */
  onPortalRequest(callback: (data: PortalRequestEvent) => void): Unsubscribe;

  /**
   * Subscribe to screencopy capture events (active/inactive notifications).
   * Fires when Wayland clients (e.g. OBS, grim) capture the screen via wlr-screencopy.
   */
  onScreencopyEvent(callback: (data: ScreencopyEvent) => void): Unsubscribe;

  /**
   * Receive a SharedArrayBuffer for a given Wayland surface.
   * Sent once when the SAB is first allocated or reallocated.
   */
  onSurfaceSab(callback: (data: SurfaceSabData) => void): Unsubscribe;

  /**
   * Subscribe to lightweight surface update signals (no pixel payload).
   * After receiving this, read pixels directly from the previously-sent SAB.
   */
  onSurfaceUpdate(callback: (data: SurfaceUpdateData) => void): Unsubscribe;
}

/** Alias for Compositor to match preload.ts imports */
export type CompositorAPI = Compositor;

/**
 * Pixel data for a captured Wayland surface
 */
export interface SurfaceBufferData {
  name: string;
  width: number;
  height: number;
  stride: number;
  pixels: Uint8Array;
  damageRects?: DamageRect[];
}

/**
 * SharedArrayBuffer registration for a Wayland surface.
 * Sent once per window when the SAB is first created or reallocated.
 */
export interface SurfaceSabData {
  name: string;
  width: number;
  height: number;
  stride: number;
  sab: SharedArrayBuffer;
}

/**
 * Lightweight surface update signal (no pixel data).
 * The renderer should read from the previously-received SAB.
 */
export interface SurfaceUpdateData {
  name: string;
  width: number;
  height: number;
  stride: number;
  damageRects?: DamageRect[];
}

/**
 * Notification data from the compositor
 */
export interface NotificationData {
  id?: string;
  title: string;
  body: string;
  icon?: string;
  timeout?: number;
  timestamp?: number;
}

declare global {
  window.woClient = {} as WoClient;
  window.compositor = {} as Compositor;
}

/**
 * Input event types from the compositor
 */
export interface MouseMoveEvent {
  type: 'pointer_motion';
  x: number;
  y: number;
  time: number;
}

export interface MouseButtonEvent {
  type: 'pointer_button';
  button: number;
  pressed: boolean;
  time: number;
}

export interface PointerLeaveEvent {
  type: 'pointer_leave';
  time: number;
}

export interface ScrollEvent {
  type: 'pointer_scroll';
  vertical: number;
  horizontal: number;
  time: number;
}

export interface KeyboardEvent {
  type: 'keyboard_key';
  key: number;
  pressed: boolean;
  time: number;
}

export type InputEvent = MouseMoveEvent | MouseButtonEvent | PointerLeaveEvent | ScrollEvent | KeyboardEvent;

export interface WindowMetadataEvent {
  type: 'windowMetadata';
  metadata: string;
}

export interface FocusChangeEvent {
  window: string;
  focused: boolean;
}

export interface RendererDamageHelperOptions {
  name: string;
  width: number;
  height: number;
  maxRects?: number;
  fullFrameThreshold?: number;
}

export interface RendererDamageHelper {
  resize(width: number, height: number): void;
  markDirty(rect: DamageRect): void;
  updateFullFrame(rgba: Uint8Array, stride?: number): void;
  updatePatch(rect: DamageRect, rgba: Uint8Array, stride?: number): void;
  captureFromCanvas(canvas: any, rects?: DamageRect[]): void;
  flush(): Promise<{ ok: boolean; seq?: string; reason?: string; skipped?: boolean }>;
}

export interface DamageRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface DamagePatch {
  rect: DamageRect;
  rgba: Uint8Array;
  stride: number;
}

export interface DamageFramePayload {
  width: number;
  height: number;
  fullFrame?: Uint8Array;
  fullStride?: number;
  patches?: DamagePatch[];
}
