import { app, BrowserWindow, ipcMain } from 'electron';
import { createConnection, createServer, Socket, Server } from 'net';
import { fileURLToPath } from 'url';
import { dirname, resolve as resolvePath, join as joinPath } from 'path';
import fs from 'fs';
import { createRequire } from 'module';
import { MAGIC, stringToBuffer } from './protocol.js';
import {
  applyDamagePayload,
  DamageBuffer,
  type DamageFramePayload,
  loadNativeDmabufModule,
  WoDmabufSender,
} from './damage-buffer.js';

// ESM __dirname equivalent
const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const require = createRequire(import.meta.url);

// Debug logging to file — buffered to avoid synchronous I/O in hot paths
const DEBUG_LOG = '/tmp/wo-electron-debug.log';
let _logBuffer: string[] = [];
let _logTimer: ReturnType<typeof setTimeout> | null = null;
function _flushLog() {
  if (_logBuffer.length === 0) return;
  try {
    fs.appendFileSync(DEBUG_LOG, _logBuffer.join(''));
  } catch (e) {
    // ignore
  }
  _logBuffer = [];
  _logTimer = null;
}
function debugLog(...args: any[]) {
  const msg = `[${new Date().toISOString()}] ` + args.join(' ') + '\n';
  _logBuffer.push(msg);
  if (!_logTimer) {
    _logTimer = setTimeout(_flushLog, 100);
  }
  console.log(...args);
}

debugLog('[Wo] Electron starting, NODE_VERSION=', process.version);

// GPU acceleration & rendering performance flags (must be set before app 'ready')
// Note: core flags (--in-process-gpu, --use-gl=egl, --ozone-platform=wayland)
// are set on the command line in the Rust spawn().  Flags below augment those.
app.commandLine.appendSwitch('ignore-gpu-blocklist');
app.commandLine.appendSwitch('enable-gpu-rasterization');
app.commandLine.appendSwitch('enable-zero-copy');
app.commandLine.appendSwitch('enable-native-gpu-memory-buffers');
app.commandLine.appendSwitch('disable-software-rasterizer');
app.commandLine.appendSwitch('enable-features',
  'CanvasOopRasterization,Vulkan,VaapiVideoDecoder,VaapiVideoEncoder,SharedArrayBuffer,RawDraw');
app.commandLine.appendSwitch('enable-accelerated-video-decode');
app.commandLine.appendSwitch('disable-frame-rate-limit');
app.commandLine.appendSwitch('disable-gpu-vsync');
debugLog('[Wo] GPU acceleration flags applied');

type ClientWindowInfo = {
  id: string;
  title: string;
  width: number;
  height: number;
};

const IPC_SOCKET = process.env.WO_IPC_SOCKET || '/run/user/1000/wo-ipc.sock';
debugLog('[Wo] WO_WINDOW_CONFIG env =', process.env.WO_WINDOW_CONFIG);
const WINDOW_CONFIG = JSON.parse(process.env.WO_WINDOW_CONFIG || '{}');
debugLog('[Wo] WINDOW_CONFIG parsed =', JSON.stringify(WINDOW_CONFIG));
const CLIENT_MODE = process.env.WO_CLIENT_MODE === '1';

const {
  name = 'default',
  url = null,
  html = null,
  width = 1920,
  height = 1080,
} = WINDOW_CONFIG;

let mainWindow: BrowserWindow | null = null;
let ipcSocket: Socket | null = null;
let portalUiServer: Server | null = null;
type PendingPortalRequest = {
  socket: Socket;
  kind: 'screen_share';
  sessionId: string;
};
const portalPendingRequests = new Map<string, PendingPortalRequest>();
const PORTAL_UI_SOCKET = '/tmp/wo-portal-ui.sock';
let ipcConnected = false;
let ipcReconnectTimer: NodeJS.Timeout | null = null;
let windowPositionUpdateTimer: NodeJS.Timeout | null = null;
let dmabufSender: WoDmabufSender | null = null;
let damageBuffer: DamageBuffer | null = null;
let nativeDmabuf: any = null;
// IPC receive buffer — grows as needed, avoids Buffer.concat on every chunk.
let compositorRxBuffer = Buffer.alloc(64 * 1024);
let rxWriteOffset = 0; // next write position
let rxReadOffset = 0;  // next read position
// ── Zero-copy surface buffer transfer via SharedArrayBuffer ──
// One SAB per window.  The main process mmap's compositor memfd data into
// the SAB; the renderer reads directly from the same memory — no
// structured-clone serialization across the Electron IPC boundary.
type SabEntry = {
  sab: SharedArrayBuffer;
  width: number;
  height: number;
  stride: number;
};
const surfaceSabCache = new Map<string, SabEntry>();
// Lightweight generation counter + damage rects sent via IPC instead of pixels
const surfaceUpdatePending = new Map<string, {
  name: string;
  width: number;
  height: number;
  stride: number;
  generation: number;
  damageRects?: Array<{x: number; y: number; width: number; height: number}>;
}>();
let surfaceUpdateGeneration = 0;
let surfaceUpdateFlushScheduled = false;
const shmFdCache = new Map<string, { pid: number; fd: number; extFd: number }>();

/** Append incoming chunk to the rx ring buffer, growing if needed. */
function rxAppend(chunk: Buffer) {
  const needed = rxWriteOffset + chunk.length;
  if (needed > compositorRxBuffer.length) {
    // Compact first: move unread data to front
    if (rxReadOffset > 0) {
      compositorRxBuffer.copy(compositorRxBuffer, 0, rxReadOffset, rxWriteOffset);
      rxWriteOffset -= rxReadOffset;
      rxReadOffset = 0;
    }
    // Grow if still needed
    if (rxWriteOffset + chunk.length > compositorRxBuffer.length) {
      const next = Buffer.alloc(Math.max(compositorRxBuffer.length * 2, rxWriteOffset + chunk.length));
      compositorRxBuffer.copy(next, 0, 0, rxWriteOffset);
      compositorRxBuffer = next;
    }
  }
  chunk.copy(compositorRxBuffer, rxWriteOffset);
  rxWriteOffset += chunk.length;
}

/** Number of unread bytes in the rx buffer. */
function rxAvailable(): number {
  return rxWriteOffset - rxReadOffset;
}

/** Consume `n` bytes from the front of the rx buffer. */
function rxConsume(n: number) {
  rxReadOffset += n;
  // Compact when the read head passes the halfway point
  if (rxReadOffset > compositorRxBuffer.length / 2) {
    compositorRxBuffer.copy(compositorRxBuffer, 0, rxReadOffset, rxWriteOffset);
    rxWriteOffset -= rxReadOffset;
    rxReadOffset = 0;
  }
}

function closeShmFdCache() {
  for (const entry of shmFdCache.values()) {
    try {
      fs.closeSync(entry.extFd);
    } catch {
      // ignore close errors
    }
  }
  shmFdCache.clear();
}

function getOrOpenShmFd(windowName: string, pid: number, fd: number): number {
  const cached = shmFdCache.get(windowName);
  if (cached && cached.pid === pid && cached.fd === fd) {
    return cached.extFd;
  }

  if (cached) {
    try {
      fs.closeSync(cached.extFd);
    } catch {
      // ignore
    }
  }

  const fdPath = `/proc/${pid}/fd/${fd}`;
  const extFd = fs.openSync(fdPath, 'r');
  shmFdCache.set(windowName, { pid, fd, extFd });
  return extFd;
}

function getOrCreateSab(windowName: string, width: number, height: number, stride: number): SabEntry {
  const existing = surfaceSabCache.get(windowName);
  const neededSize = stride * height;
  if (existing && existing.sab.byteLength >= neededSize && existing.width === width && existing.height === height && existing.stride === stride) {
    return existing;
  }
  const sab = new SharedArrayBuffer(neededSize);
  const entry: SabEntry = { sab, width, height, stride };
  surfaceSabCache.set(windowName, entry);
  // Tell renderer about the new SAB so it can reference it
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.postMessage('wo:surface-sab', {
      name: windowName,
      sab,
      width,
      height,
      stride,
    });
  }
  return entry;
}

function flushSurfaceUpdates() {
  surfaceUpdateFlushScheduled = false;
  if (!mainWindow || mainWindow.isDestroyed()) { surfaceUpdatePending.clear(); return; }
  for (const entry of surfaceUpdatePending.values()) {
    // Send only metadata (no pixels) — the renderer reads from SAB directly
    mainWindow.webContents.send('wo:surface-update', entry);
  }
  surfaceUpdatePending.clear();
}
const inFlightFrameSeqs = new Set<string>();
const MAX_IN_FLIGHT_FRAMES = 3;
// When the app uses compositor.submitDamageFrame() directly, skip OSR paint path to avoid double submission
let appUsedDamageHelper = false;
// Track pointer position for mouse button/scroll events (window-local coords)
let pointerX = 0;
let pointerY = 0;

function respondToPortalRequest(requestId: string, params: {
  allowed: boolean;
  type?: string;
  windowName?: unknown;
}): { ok: boolean; error?: string } {
  const pending = portalPendingRequests.get(requestId);
  if (!pending) {
    return { ok: false, error: `No pending portal request: ${requestId}` };
  }

  let response: Record<string, unknown>;
  if (pending.kind === 'screen_share') {
    response = {
      allowed: Boolean(params.allowed),
      sourceType: params.type === 'window' ? 'Window' : 'Monitor',
      windowName: typeof params.windowName === 'string' ? params.windowName : null,
    };
  } else {
    response = { allowed: false };
  }

  try {
    pending.socket.write(`${JSON.stringify(response)}\n`);
    pending.socket.end();
    portalPendingRequests.delete(requestId);
    return { ok: true };
  } catch (error) {
    portalPendingRequests.delete(requestId);
    return { ok: false, error: `Failed to send portal response: ${String(error)}` };
  }
}

function startPortalUiBridge() {
  try {
    if (fs.existsSync(PORTAL_UI_SOCKET)) {
      fs.unlinkSync(PORTAL_UI_SOCKET);
    }
  } catch {
    // ignore stale socket cleanup errors
  }

  portalUiServer = createServer((socket) => {
    let received = '';

    socket.on('data', (chunk: Buffer) => {
      received += chunk.toString('utf8');
      const nl = received.indexOf('\n');
      if (nl < 0) return;

      const line = received.slice(0, nl).trim();
      received = received.slice(nl + 1);
      if (!line) return;

      try {
        const req = JSON.parse(line) as {
          type?: string;
          appName?: string;
          sessionId?: string;
        };

        if (req.type !== 'screen_share_request' || !req.sessionId) {
          socket.end('{"allowed":false,"reason":"invalid_request"}\n');
          return;
        }

        const requestId = req.sessionId;

        portalPendingRequests.set(requestId, {
          socket,
          kind: 'screen_share',
          sessionId: req.sessionId,
        });

        // Auto-deny if UI does not respond within 90s
        socket.setTimeout(90_000, () => {
          const pending = portalPendingRequests.get(requestId);
          if (pending) {
            portalPendingRequests.delete(requestId);
            pending.socket.end('{"allowed":false,"reason":"timeout"}\n');
          }
        });

        if (mainWindow && !mainWindow.isDestroyed()) {
          const payload = {
            requestId,
            kind: 'screen_share',
            appName: req.appName || 'Application',
            sessionId: req.sessionId,
          };

          mainWindow.webContents.send('wo:portal-request', payload);
        } else {
          portalPendingRequests.delete(requestId);
          socket.end('{"allowed":false,"reason":"no_ui"}\n');
        }
      } catch {
        socket.end('{"allowed":false,"reason":"invalid_json"}\n');
      }
    });

    socket.on('close', () => {
      for (const [sessionId, pending] of portalPendingRequests.entries()) {
        if (pending.socket === socket) {
          portalPendingRequests.delete(sessionId);
          break;
        }
      }
    });

    socket.on('error', () => {
      // socket errors are expected if requester exits early
    });
  });

  portalUiServer.listen(PORTAL_UI_SOCKET, () => {
    debugLog('[Wo] Portal UI bridge listening at', PORTAL_UI_SOCKET);
  });

  portalUiServer.on('error', (err) => {
    console.error('[Wo] Portal UI bridge error:', err);
  });
}

function stopPortalUiBridge() {
  for (const pending of portalPendingRequests.values()) {
    try {
      pending.socket.end('{"allowed":false,"reason":"shutdown"}\n');
    } catch {
      // ignore
    }
  }
  portalPendingRequests.clear();

  if (portalUiServer) {
    portalUiServer.close();
    portalUiServer = null;
  }

  try {
    if (fs.existsSync(PORTAL_UI_SOCKET)) {
      fs.unlinkSync(PORTAL_UI_SOCKET);
    }
  } catch {
    // ignore socket cleanup errors
  }
}

function parseClientWindows(): ClientWindowInfo[] {
  try {
    const raw = process.env.WO_CLIENT_WINDOWS;
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed) && parsed.length > 0) {
        return parsed.map((w: any, idx: number) => ({
          id: String(w.id ?? w.name ?? `win-${idx + 1}`),
          title: String(w.title ?? w.name ?? `Window ${idx + 1}`),
          width: Number(w.width ?? 960),
          height: Number(w.height ?? 540),
        }));
      }
    }
  } catch (error) {
    console.warn('[Wo client] invalid WO_CLIENT_WINDOWS:', error);
  }

  return [{ id: String(name), title: String(name), width: Number(width), height: Number(height) }];
}

let clientWindows: ClientWindowInfo[] = parseClientWindows();
// Live window list received from the compositor via WOWM metadata messages.
let compositorWindows: Record<string, unknown>[] = [];

type StatusNotifierTrayItem = {
  id: string;
  title: string;
  status: 'active' | 'passive' | 'attention';
  icon: string;
  iconDataUrl?: string;
  service: string;
  objectPath: string;
  menuPath?: string;
  hasMenu: boolean;
};

function parseQuotedStrings(output: string): string[] {
  const matches = output.matchAll(/"((?:[^"\\]|\\.)*)"/g);
  const values: string[] = [];
  for (const match of matches) {
    const raw = match[1];
    values.push(raw.replace(/\\"/g, '"').replace(/\\\\/g, '\\'));
  }
  return values;
}

function runBusctlUser(args: string[]): string {
  try {
    const { execFileSync } = require('child_process') as typeof import('child_process');
    return execFileSync('busctl', ['--user', ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return '';
  }
}

function parseServiceAndPath(itemRef: string): { service: string; objectPath: string } | null {
  const slashIdx = itemRef.indexOf('/');
  if (slashIdx > 0) {
    return {
      service: itemRef.slice(0, slashIdx),
      objectPath: itemRef.slice(slashIdx),
    };
  }

  if (!itemRef) {
    return null;
  }

  return {
    service: itemRef,
    objectPath: '/StatusNotifierItem',
  };
}

function parseBusctlStringValue(output: string): string {
  const quoted = parseQuotedStrings(output);
  if (quoted.length > 0) {
    return quoted[0];
  }
  const parts = output.split(/\s+/);
  return parts.length >= 2 ? parts.slice(1).join(' ').trim() : '';
}

function getBusProperty(service: string, objectPath: string, iface: string, prop: string): string {
  const out = runBusctlUser(['get-property', service, objectPath, iface, prop]);
  return out ? parseBusctlStringValue(out) : '';
}

function mapTrayIcon(iconName: string, title: string): string {
  const n = iconName.toLowerCase();
  const t = title.toLowerCase();
  if (n.includes('network') || t.includes('network') || t.includes('wifi')) return 'mdi:wifi';
  if (n.includes('audio') || n.includes('volume') || t.includes('volume')) return 'mdi:volume-high';
  if (n.includes('battery') || t.includes('battery')) return 'mdi:battery';
  if (n.includes('bluetooth') || t.includes('bluetooth')) return 'mdi:bluetooth';
  if (n.includes('telegram') || t.includes('telegram')) return 'mdi:telegram';
  if (n.includes('discord') || t.includes('discord')) return 'mdi:discord';
  if (n.includes('steam') || t.includes('steam')) return 'mdi:steam';
  if (n.includes('dropbox') || t.includes('dropbox')) return 'mdi:dropbox';
  if (n.includes('mail') || t.includes('mail')) return 'mdi:email-outline';
  if (n.includes('kde') || t.includes('kde')) return 'mdi:kde';
  return 'mdi:circle-medium';
}

function iconPathCandidates(iconName: string): string[] {
  const direct = iconName.startsWith('/') ? [iconName] : [];
  if (iconName.startsWith('/')) {
    return direct;
  }

  const exts = ['png', 'svg'];
  const bases = [
    '/usr/share/pixmaps',
    '/usr/share/icons/hicolor/16x16/apps',
    '/usr/share/icons/hicolor/22x22/apps',
    '/usr/share/icons/hicolor/24x24/apps',
    '/usr/share/icons/hicolor/32x32/apps',
    '/usr/share/icons/hicolor/48x48/apps',
    '/usr/share/icons/hicolor/scalable/apps',
    '/usr/share/icons/breeze/apps/22',
    '/usr/share/icons/breeze/apps/24',
    '/usr/share/icons/breeze/apps/32',
  ];

  const results: string[] = [];
  for (const base of bases) {
    for (const ext of exts) {
      results.push(`${base}/${iconName}.${ext}`);
    }
  }
  return results;
}

function toDataUrlForIconFile(iconPath: string): string | undefined {
  try {
    if (!fs.existsSync(iconPath)) {
      return undefined;
    }
    const lower = iconPath.toLowerCase();
    const mime = lower.endsWith('.svg') ? 'image/svg+xml' : lower.endsWith('.png') ? 'image/png' : '';
    if (!mime) {
      return undefined;
    }
    const base64 = fs.readFileSync(iconPath).toString('base64');
    return `data:${mime};base64,${base64}`;
  } catch {
    return undefined;
  }
}

function resolveTrayIconDataUrl(iconName: string): string | undefined {
  for (const candidate of iconPathCandidates(iconName)) {
    const dataUrl = toDataUrlForIconFile(candidate);
    if (dataUrl) {
      return dataUrl;
    }
  }
  return undefined;
}

function getStatusNotifierTrayItems(): StatusNotifierTrayItem[] {
  const watcherCall = runBusctlUser([
    'call',
    'org.kde.StatusNotifierWatcher',
    '/StatusNotifierWatcher',
    'org.kde.StatusNotifierWatcher',
    'RegisteredStatusNotifierItems',
  ]);

  if (!watcherCall) {
    return [];
  }

  const refs = parseQuotedStrings(watcherCall);
  const results: StatusNotifierTrayItem[] = [];

  for (const itemRef of refs) {
    const parsed = parseServiceAndPath(itemRef);
    if (!parsed) {
      continue;
    }

    const statusRaw = getBusProperty(parsed.service, parsed.objectPath, 'org.kde.StatusNotifierItem', 'Status');
    const title =
      getBusProperty(parsed.service, parsed.objectPath, 'org.kde.StatusNotifierItem', 'Title')
      || getBusProperty(parsed.service, parsed.objectPath, 'org.kde.StatusNotifierItem', 'Id')
      || parsed.service;
    const iconName =
      getBusProperty(parsed.service, parsed.objectPath, 'org.kde.StatusNotifierItem', 'AttentionIconName')
      || getBusProperty(parsed.service, parsed.objectPath, 'org.kde.StatusNotifierItem', 'IconName');
    const menuPath = getBusProperty(parsed.service, parsed.objectPath, 'org.kde.StatusNotifierItem', 'Menu');
    const iconDataUrl = iconName ? resolveTrayIconDataUrl(iconName) : undefined;

    let status: StatusNotifierTrayItem['status'] = 'active';
    const s = statusRaw.toLowerCase();
    if (s.includes('passive')) {
      status = 'passive';
    } else if (s.includes('attention') || s.includes('needsattention')) {
      status = 'attention';
    }

    results.push({
      id: `${parsed.service}${parsed.objectPath}`,
      title,
      status,
      icon: mapTrayIcon(iconName || title, title),
      iconDataUrl,
      service: parsed.service,
      objectPath: parsed.objectPath,
      menuPath: menuPath || undefined,
      hasMenu: Boolean(menuPath && menuPath !== '/NO_DBUSMENU'),
    });
  }

  return results;
}

function activateStatusNotifierItem(service: string, objectPath: string): boolean {
  const out = runBusctlUser([
    'call',
    service,
    objectPath,
    'org.kde.StatusNotifierItem',
    'Activate',
    'ii',
    '0',
    '0',
  ]);
  return out.length > 0;
}

function secondaryActivateStatusNotifierItem(service: string, objectPath: string): boolean {
  const out = runBusctlUser([
    'call',
    service,
    objectPath,
    'org.kde.StatusNotifierItem',
    'SecondaryActivate',
    'ii',
    '0',
    '0',
  ]);
  return out.length > 0;
}

function openStatusNotifierContextMenu(service: string, objectPath: string, x: number, y: number): boolean {
  const out = runBusctlUser([
    'call',
    service,
    objectPath,
    'org.kde.StatusNotifierItem',
    'ContextMenu',
    'ii',
    String(Math.trunc(x)),
    String(Math.trunc(y)),
  ]);
  return out.length > 0;
}

type DBusMenuItem = {
  id: string;
  label: string;
  enabled: boolean;
  children?: DBusMenuItem[];
};

function getDBusMenuLayout(service: string, menuPath: string): DBusMenuItem[] {
  try {
    const out = runBusctlUser([
      'call',
      service,
      menuPath.startsWith('/') ? menuPath : '/com/canonical/dbusmenu',
      'com.canonical.dbusmenu',
      'GetLayout',
      'u',
      '0',
    ]);

    if (!out) {
      return [];
    }

    const lines = out.split('\n').filter(l => l.trim().length > 0);
    const items: DBusMenuItem[] = [];
    let depth = 0;

    for (const line of lines) {
      const trimmed = line.trim();
      const newDepth = (line.match(/^\s*/)?.[0]?.length ?? 0) / 2;
      
      if (trimmed.startsWith('(') && trimmed.includes('"')) {
        const quoted = parseQuotedStrings(trimmed);
        if (quoted.length > 0) {
          const label = quoted[0];
          const item: DBusMenuItem = {
            id: label,
            label,
            enabled: !line.toLowerCase().includes('disabled'),
          };

          if (newDepth === 0) {
            items.push(item);
          } else if (items.length > 0 && newDepth > depth) {
            const parent = items[items.length - 1];
            if (!parent.children) {
              parent.children = [];
            }
            parent.children.push(item);
          }
        }
        depth = newDepth;
      }
    }

    return items;
  } catch {
    return [];
  }
}

function triggerDBusMenuEvent(service: string, menuPath: string, menuId: string): boolean {
  try {
    const out = runBusctlUser([
      'call',
      service,
      menuPath.startsWith('/') ? menuPath : '/com/canonical/dbusmenu',
      'com.canonical.dbusmenu',
      'Event',
      'sis',
      menuId,
      'clicked',
      '',
    ]);
    return out.length > 0;
  } catch {
    return false;
  }
}

function getSocketFd(socket: Socket): number {
  const fd = (socket as any)?._handle?.fd;
  if (typeof fd !== 'number' || !Number.isFinite(fd) || fd < 0) {
    throw new Error('Unable to read underlying socket file descriptor');
  }
  return fd;
}

function scheduleIpcReconnect(delayMs = 1000) {
  if (CLIENT_MODE || process.env.WO_STANDALONE === '1') {
    return;
  }
  if (ipcReconnectTimer) {
    return;
  }

  ipcReconnectTimer = setTimeout(() => {
    ipcReconnectTimer = null;
    connectToCompositor()
      .then(() => {
        debugLog('[Wo] IPC reconnect succeeded');
      })
      .catch((error) => {
        debugLog('[Wo] IPC reconnect failed:', String(error));
        scheduleIpcReconnect();
      });
  }, delayMs);
}

function sendActionToCompositor(action: string, payload?: Record<string, unknown>): boolean {
  if (!ipcSocket || !ipcConnected) {
    return false;
  }

  try {
    const actionBuf = stringToBuffer(action);
    const payloadStr = payload ? JSON.stringify(payload) : '';
    const payloadBuf = stringToBuffer(payloadStr);

    const messageBuf = Buffer.alloc(12 + actionBuf.length + payloadBuf.length);
    let offset = 0;
    messageBuf.writeUInt32LE(MAGIC.ACTION, offset); offset += 4;
    messageBuf.writeUInt32LE(actionBuf.length, offset); offset += 4;
    actionBuf.copy(messageBuf, offset); offset += actionBuf.length;
    messageBuf.writeUInt32LE(payloadBuf.length, offset); offset += 4;
    payloadBuf.copy(messageBuf, offset);

    ipcSocket.write(messageBuf);
    return true;
  } catch {
    return false;
  }
}

function sendWindowPositionUpdate(x: number, y: number, width: number, height: number): boolean {
  if (!ipcSocket || !ipcConnected) {
    return false;
  }

  try {
    const nameBuf = stringToBuffer(name);
    const messageBuf = Buffer.alloc(8 + nameBuf.length + 16);
    
    let offset = 0;
    messageBuf.writeUInt32LE(MAGIC.WINDOW_POS, offset); offset += 4;
    messageBuf.writeUInt32LE(nameBuf.length, offset); offset += 4;
    nameBuf.copy(messageBuf, offset); offset += nameBuf.length;
    messageBuf.writeInt32LE(x, offset); offset += 4;
    messageBuf.writeInt32LE(y, offset); offset += 4;
    messageBuf.writeUInt32LE(width, offset); offset += 4;
    messageBuf.writeUInt32LE(height, offset);

    ipcSocket.write(messageBuf);
    return true;
  } catch {
    return false;
  }
}

function sendForwardedKeyboard(windowName: string, evdevKey: number, pressed: boolean, time: number): boolean {
  if (!ipcSocket || !ipcConnected) {
    return false;
  }

  try {
    const nameBuf = stringToBuffer(windowName);
    // wire: magic(4) + name_len(4) + name(N) + key(4) + pressed(4) + time(4)
    const messageBuf = Buffer.alloc(8 + nameBuf.length + 12);
    let offset = 0;
    messageBuf.writeUInt32LE(MAGIC.FORWARD_KEYBOARD, offset); offset += 4;
    messageBuf.writeUInt32LE(nameBuf.length, offset); offset += 4;
    nameBuf.copy(messageBuf, offset); offset += nameBuf.length;
    messageBuf.writeUInt32LE(evdevKey, offset); offset += 4;
    messageBuf.writeUInt32LE(pressed ? 1 : 0, offset); offset += 4;
    messageBuf.writeUInt32LE(time, offset);

    ipcSocket.write(messageBuf);
    return true;
  } catch {
    return false;
  }
}

// Map Linux evdev button code to Electron mouse button name
function linuxButtonToElectron(btn: number): 'left' | 'middle' | 'right' {
  if (btn === 273) return 'right';
  if (btn === 274) return 'middle';
  return 'left'; // 272 = BTN_LEFT and default
}

// Modifier state tracked from key events
const modifierState = { shift: false, control: false, alt: false, meta: false };

// Evdev keycodes that correspond to modifier keys
const MODIFIER_KEYCODES: Record<number, keyof typeof modifierState> = {
  29: 'control',  // KEY_LEFTCTRL
  42: 'shift',    // KEY_LEFTSHIFT
  54: 'shift',    // KEY_RIGHTSHIFT
  56: 'alt',      // KEY_LEFTALT
  97: 'control',  // KEY_RIGHTCTRL
  100: 'alt',     // KEY_RIGHTALT
  125: 'meta',    // KEY_LEFTMETA
  126: 'meta',    // KEY_RIGHTMETA
};

type InputModifier = 'shift' | 'control' | 'alt' | 'meta';

function getModifierArray(): InputModifier[] {
  const mods: InputModifier[] = [];
  if (modifierState.shift) mods.push('shift');
  if (modifierState.control) mods.push('control');
  if (modifierState.alt) mods.push('alt');
  if (modifierState.meta) mods.push('meta');
  return mods;
}

// Shifted character mapping for generating correct char events
const SHIFTED_CHARS: Record<string, string> = {
  '1': '!', '2': '@', '3': '#', '4': '$', '5': '%', '6': '^', '7': '&', '8': '*', '9': '(', '0': ')',
  '-': '_', '=': '+', '[': '{', ']': '}', '\\': '|', ';': ':', "'": '"', '`': '~',
  ',': '<', '.': '>', '/': '?',
};

// Map Linux evdev keycode to Electron sendInputEvent keyCode string
const LINUX_KEYCODE_MAP: Record<number, string> = {
  // Row 1: Escape and function keys
  1: 'Escape',
  59: 'F1', 60: 'F2', 61: 'F3', 62: 'F4', 63: 'F5', 64: 'F6',
  65: 'F7', 66: 'F8', 67: 'F9', 68: 'F10', 87: 'F11', 88: 'F12',

  // Row 2: Number row
  41: '`', 2: '1', 3: '2', 4: '3', 5: '4', 6: '5', 7: '6', 8: '7', 9: '8', 10: '9',
  11: '0', 12: '-', 13: '=', 14: 'Backspace',

  // Row 3: QWERTY
  15: 'Tab',
  16: 'q', 17: 'w', 18: 'e', 19: 'r', 20: 't', 21: 'y', 22: 'u', 23: 'i', 24: 'o', 25: 'p',
  26: '[', 27: ']', 43: '\\',

  // Row 4: Home row
  58: 'CapsLock',
  30: 'a', 31: 's', 32: 'd', 33: 'f', 34: 'g', 35: 'h', 36: 'j', 37: 'k', 38: 'l',
  39: ';', 40: "'", 28: 'Return',

  // Row 5: Bottom row
  42: 'Shift',
  44: 'z', 45: 'x', 46: 'c', 47: 'v', 48: 'b', 49: 'n', 50: 'm',
  51: ',', 52: '.', 53: '/', 54: 'Shift',

  // Row 6: Modifiers and space
  29: 'Control', 125: 'Meta', 56: 'Alt', 57: 'Space', 100: 'Alt', 126: 'Meta', 97: 'Control',

  // Navigation cluster
  110: 'Insert', 102: 'Home', 104: 'PageUp',
  111: 'Delete', 107: 'End', 109: 'PageDown',

  // Arrow keys
  103: 'Up', 105: 'Left', 108: 'Down', 106: 'Right',

  // Numpad
  69: 'NumLock', 98: 'numdiv', 55: 'nummult', 74: 'numsub',
  71: 'num7', 72: 'num8', 73: 'num9', 78: 'numadd',
  75: 'num4', 76: 'num5', 77: 'num6',
  79: 'num1', 80: 'num2', 81: 'num3', 96: 'Enter',
  82: 'num0', 83: 'numdec',

  // Media / misc
  99: 'PrintScreen', 70: 'ScrollLock', 119: 'Pause',
  113: 'VolumeMute', 114: 'VolumeDown', 115: 'VolumeUp',
  163: 'MediaNextTrack', 165: 'MediaPreviousTrack', 164: 'MediaPlayPause', 166: 'MediaStop',
};

function connectToCompositor(): Promise<void> {
  return new Promise((resolve, reject) => {
    if (ipcReconnectTimer) {
      clearTimeout(ipcReconnectTimer);
      ipcReconnectTimer = null;
    }
    if (ipcSocket && !ipcSocket.destroyed) {
      ipcSocket.destroy();
    }

    debugLog('[Wo] Connecting to compositor IPC:', IPC_SOCKET);
    ipcSocket = createConnection(IPC_SOCKET, () => {
      const windowName = name;
      const nameBuf = stringToBuffer(windowName);
      const messageBuf = Buffer.alloc(8 + nameBuf.length + 8);

      let offset = 0;
      messageBuf.writeUInt32LE(MAGIC.HELLO, offset); offset += 4;
      messageBuf.writeUInt32LE(nameBuf.length, offset); offset += 4;
      nameBuf.copy(messageBuf, offset); offset += nameBuf.length;
      messageBuf.writeUInt32LE(width, offset); offset += 4;
      messageBuf.writeUInt32LE(height, offset);

      ipcSocket!.write(messageBuf);
      ipcConnected = true;
      debugLog('[Wo] IPC connected, sent HELLO message');

      try {
        const nativePath = resolvePath(__dirname, '../native/build/Release/wo_dmabuf.node');
        nativeDmabuf = loadNativeDmabufModule(require, nativePath);
        nativeDmabuf.init(process.env.WO_DRM_RENDER_NODE || '/dev/dri/renderD128');
        dmabufSender = new WoDmabufSender(ipcSocket!, name, nativeDmabuf);
        debugLog('[Wo] DMABUF sender initialized');
      } catch (error) {
        debugLog('[Wo] DMABUF sender unavailable:', error);
        dmabufSender = null;
      }

      resolve();
    });

    let ipcDataCalls = 0;
    let ipcDataLastLog = Date.now();
    // Input event batching for improved responsiveness
    let pendingMouseMove: { x: number; y: number } | null = null;
    let inputFlushTimer: NodeJS.Timeout | null = null;
    const flushInputEvents = () => {
      inputFlushTimer = null;
      if (pendingMouseMove && mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.sendInputEvent({
          type: 'mouseMove',
          x: pendingMouseMove.x,
          y: pendingMouseMove.y,
        });
        pendingMouseMove = null;
      }
    };
    
    ipcSocket.on('data', (chunk: Buffer) => {
      ipcDataCalls++;
      const now = Date.now();
      if (now - ipcDataLastLog > 2000) {
        ipcDataCalls = 0;
        ipcDataLastLog = now;
      }
      const dataT0 = Date.now();
      rxAppend(chunk);
      while (rxAvailable() >= 4) {
        const magic = compositorRxBuffer.readUInt32LE(rxReadOffset);

        if (magic === MAGIC.FRAME_ACK) {
          if (rxAvailable() < 12) break;
          const seq = compositorRxBuffer.readBigUInt64LE(rxReadOffset + 4).toString();
          inFlightFrameSeqs.delete(seq);
          rxConsume(12);
          continue;
        }

        if (magic === MAGIC.MOUSE_MOVE) {
          // magic(4) + x(8, f64) + y(8, f64) = 20 bytes
          if (rxAvailable() < 20) break;
          const x = compositorRxBuffer.readDoubleLE(rxReadOffset + 4);
          const y = compositorRxBuffer.readDoubleLE(rxReadOffset + 12);
          rxConsume(20);
          pointerX = Math.round(x);
          pointerY = Math.round(y);
          
          // OPTIMIZATION: Batch mouse moves for responsiveness
          // Flush immediately if no timer pending, otherwise coalesce
          if (!inputFlushTimer) {
            if (mainWindow && !mainWindow.isDestroyed()) {
              mainWindow.webContents.sendInputEvent({ type: 'mouseMove', x: pointerX, y: pointerY });
            }
          } else {
            // Coalesce subsequent moves
            pendingMouseMove = { x: pointerX, y: pointerY };
            inputFlushTimer = setTimeout(flushInputEvents, 0);
          }
          continue;
        }

        if (magic === MAGIC.MOUSE_BUTTON) {
          // magic(4) + button(4) + pressed(4) + time(4) = 16 bytes
          if (rxAvailable() < 16) break;
          const button = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const pressed = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          rxConsume(16);
          
          // OPTIMIZATION: Flush pending mouse moves before button events
          if (pendingMouseMove && mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.sendInputEvent({
              type: 'mouseMove',
              x: pendingMouseMove.x,
              y: pendingMouseMove.y,
            });
            pendingMouseMove = null;
          }
          if (inputFlushTimer) {
            clearTimeout(inputFlushTimer);
            inputFlushTimer = null;
          }
          
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.sendInputEvent({
              type: pressed ? 'mouseDown' : 'mouseUp',
              x: pointerX,
              y: pointerY,
              button: linuxButtonToElectron(button),
              clickCount: 1,
              modifiers: getModifierArray(),
            });
          }
          continue;
        }

        if (magic === MAGIC.KEYBOARD) {
          // magic(4) + key(4) + pressed(4) + time(4) = 16 bytes
          if (rxAvailable() < 16) break;
          const key = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const pressed = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          rxConsume(16);

          // Update modifier state before generating events
          const modKey = MODIFIER_KEYCODES[key];
          if (modKey) {
            modifierState[modKey] = pressed;
          }

          const keyCode = LINUX_KEYCODE_MAP[key];
          if (keyCode && mainWindow && !mainWindow.isDestroyed()) {
            const modifiers = getModifierArray();
            mainWindow.webContents.sendInputEvent({
              type: pressed ? 'keyDown' : 'keyUp',
              keyCode,
              modifiers,
            });
            // Send char event for printable keys on press
            if (pressed && keyCode.length === 1) {
              let charKey = keyCode;
              if (modifierState.shift) {
                if (charKey >= 'a' && charKey <= 'z') {
                  charKey = charKey.toUpperCase();
                } else if (SHIFTED_CHARS[charKey]) {
                  charKey = SHIFTED_CHARS[charKey];
                }
              }
              mainWindow.webContents.sendInputEvent({ type: 'char', keyCode: charKey, modifiers });
            }
          }
          continue;
        }

        if (magic === MAGIC.SCROLL) {
          // magic(4) + vertical(4, i32) + horizontal(4, i32) + time(4) = 16 bytes
          if (rxAvailable() < 16) break;
          const vertical = compositorRxBuffer.readInt32LE(rxReadOffset + 4);
          const horizontal = compositorRxBuffer.readInt32LE(rxReadOffset + 8);
          rxConsume(16);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.sendInputEvent({
              type: 'mouseWheel',
              x: pointerX,
              y: pointerY,
              deltaX: horizontal,
              deltaY: vertical,
              canScroll: true,
            });
          }
          continue;
        }

        if (magic === MAGIC.FOCUS_CHANGE) {
          // magic(4) + name_len(4) + focused(4) + name(N) = 12 + N bytes
          if (rxAvailable() < 12) break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 12 + nameLen) break;
          const focused = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          const windowName = compositorRxBuffer.slice(rxReadOffset + 12, rxReadOffset + 12 + nameLen).toString('utf8');
          rxConsume(12 + nameLen);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send('wo:focus-change', { window: windowName, focused });
          }
          continue;
        }

        if (magic === MAGIC.WINDOW_META) {
          // magic(4) + payload_len(4) + payload(N) = 8 + N bytes
          if (rxAvailable() < 8) break;
          const payloadLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 8 + payloadLen) break;
          const metadata = compositorRxBuffer.slice(rxReadOffset + 8, rxReadOffset + 8 + payloadLen).toString('utf8');
          rxConsume(8 + payloadLen);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send('wo:window-metadata', { type: 'windowMetadata', metadata });
            // Parse and forward as structured window list for woClient.onWindows
            try {
              const parsed = JSON.parse(metadata);
              if (parsed && Array.isArray(parsed.windows)) {
                // Clean up caches for windows that disappeared
                const currentNames = new Set((parsed.windows as Array<{name?: string}>).map(w => w.name).filter(Boolean));
                for (const cachedName of surfaceSabCache.keys()) {
                  if (!currentNames.has(cachedName)) {
                    surfaceSabCache.delete(cachedName);
                  }
                }
                for (const cachedName of shmFdCache.keys()) {
                  if (!currentNames.has(cachedName)) {
                    const entry = shmFdCache.get(cachedName);
                    if (entry) {
                      try { fs.closeSync(entry.extFd); } catch { /* ignore */ }
                    }
                    shmFdCache.delete(cachedName);
                  }
                }

                compositorWindows = parsed.windows;
                mainWindow.webContents.send('wo:windows', compositorWindows);
              }
            } catch { /* ignore parse errors */ }
          }
          continue;
        }

        if (magic === MAGIC.SHM_BUFFER) {
          if (rxAvailable() < 8) break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          // Minimum header before we know num_rects
          const baseHeader = 8 + nameLen + 20 + 4; // +4 for num_rects field
          if (rxAvailable() < baseHeader) break;
          let off = rxReadOffset + 8;
          const windowName = compositorRxBuffer.slice(off, off + nameLen).toString('utf8');
          off += nameLen;
          const sbWidth = compositorRxBuffer.readUInt32LE(off); off += 4;
          const sbHeight = compositorRxBuffer.readUInt32LE(off); off += 4;
          const sbStride = compositorRxBuffer.readUInt32LE(off); off += 4;
          const pid = compositorRxBuffer.readUInt32LE(off); off += 4;
          const fd = compositorRxBuffer.readUInt32LE(off); off += 4;
          const numRects = compositorRxBuffer.readUInt32LE(off); off += 4;
          const fullHeader = baseHeader + numRects * 16;
          if (rxAvailable() < fullHeader) break;

          const damageRects: Array<{x: number; y: number; width: number; height: number}> = [];
          for (let i = 0; i < numRects; i++) {
            damageRects.push({
              x: compositorRxBuffer.readUInt32LE(off),
              y: compositorRxBuffer.readUInt32LE(off + 4),
              width: compositorRxBuffer.readUInt32LE(off + 8),
              height: compositorRxBuffer.readUInt32LE(off + 12),
            });
            off += 16;
          }
          rxConsume(fullHeader);

          try {
            const extFd = getOrOpenShmFd(windowName, pid, fd);
            const fullSize = sbStride * sbHeight;

            // Ensure a SAB exists for this window at the right dimensions
            const sabEntry = getOrCreateSab(windowName, sbWidth, sbHeight, sbStride);

            const hasUsableRects = damageRects.length > 0 && damageRects.length < 64;
            if (hasUsableRects && nativeDmabuf?.copyMmapDamageToSab) {
              // Partial damage: mmap and copy only damaged row bands into SAB
              let minY = sbHeight, maxY = 0;
              for (const r of damageRects) {
                const ry = Math.max(0, r.y);
                const ryEnd = Math.min(sbHeight, r.y + r.height);
                if (ry < minY) minY = ry;
                if (ryEnd > maxY) maxY = ryEnd;
              }
              if (minY < maxY) {
                nativeDmabuf.copyMmapDamageToSab(extFd, sabEntry.sab, sbStride, [
                  { y: minY, h: maxY - minY },
                ]);
              }
            } else if (nativeDmabuf?.copyMmapToSab) {
              // Full frame: mmap entire buffer into SAB
              nativeDmabuf.copyMmapToSab(extFd, sabEntry.sab, fullSize);
            } else {
              // Fallback: fs.readSync into a temporary buffer, then copy to SAB
              const tmpBuf = Buffer.alloc(fullSize);
              let totalRead = 0;
              while (totalRead < fullSize) {
                const bytesRead = fs.readSync(extFd, tmpBuf, totalRead, fullSize - totalRead, totalRead);
                if (bytesRead <= 0) break;
                totalRead += bytesRead;
              }
              new Uint8Array(sabEntry.sab).set(new Uint8Array(tmpBuf.buffer, tmpBuf.byteOffset, tmpBuf.byteLength));
            }

            surfaceUpdatePending.set(windowName, {
              name: windowName,
              width: sbWidth,
              height: sbHeight,
              stride: sbStride,
              generation: ++surfaceUpdateGeneration,
              damageRects: hasUsableRects ? damageRects : undefined,
            });
            if (!surfaceUpdateFlushScheduled) {
              surfaceUpdateFlushScheduled = true;
              setImmediate(flushSurfaceUpdates);
            }
          } catch (err) {
            const cached = shmFdCache.get(windowName);
            if (cached) {
              try {
                fs.closeSync(cached.extFd);
              } catch {
                // ignore
              }
              shmFdCache.delete(windowName);
            }
            debugLog(`[Wo] SHM_BUFFER fs.readSync failed for ${windowName}:`, err);
          }
          continue;
        }

        if (magic === MAGIC.DMABUF_FRAME) {
          if (rxAvailable() < 8) break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const numPlanesOffset = 8 + nameLen + 12;
          if (rxAvailable() < numPlanesOffset + 4) break;

          let off = rxReadOffset + 8;
          const dmabufName = compositorRxBuffer.slice(off, off + nameLen).toString('utf8');
          off += nameLen;
          const dmabufW = compositorRxBuffer.readUInt32LE(off); off += 4;
          const dmabufH = compositorRxBuffer.readUInt32LE(off); off += 4;
          const dmabufFormat = compositorRxBuffer.readUInt32LE(off); off += 4;
          const numPlanes = compositorRxBuffer.readUInt32LE(off); off += 4;

          const totalSize = (off - rxReadOffset) + (numPlanes * 24);
          if (rxAvailable() < totalSize) break;

          try {
            if (nativeDmabuf && ipcSocket) {
              const socketFd = (ipcSocket as any)?._handle?.fd;
              if (typeof socketFd === 'number' && socketFd >= 0) {
                const dmabufFd = nativeDmabuf.recvFd(socketFd);
                if (dmabufFd >= 0) {
                  try {
                    const textureInfo = nativeDmabuf.importDmabufTexture(dmabufName, dmabufFd, dmabufW, dmabufH, dmabufFormat);
                    if (mainWindow && !mainWindow.isDestroyed()) {
                      mainWindow.webContents.send('wo:dmabuf-frame', {
                        name: dmabufName,
                        texture: textureInfo.texture,
                        width: textureInfo.width,
                        height: textureInfo.height,
                      });
                    }
                  } finally {
                    fs.closeSync(dmabufFd);
                  }
                }
              }
            }
          } catch (err) {
            debugLog(`[Wo] DMABUF import failed for ${dmabufName}:`, err);
          }

          rxConsume(totalSize);
          continue;
        }

        if (magic === MAGIC.POINTER_LOCK_REQUEST) {
          // magic(4) + name_len(4) + lock(4) + name(N) = 12 + N bytes
          if (rxAvailable() < 12) break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 12 + nameLen) break;
          const lock = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          const windowName = compositorRxBuffer.slice(rxReadOffset + 12, rxReadOffset + 12 + nameLen).toString('utf8');
          rxConsume(12 + nameLen);

          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send('wo:pointer-lock-request', { window: windowName, lock });
          }
          continue;
        }

        if (magic === MAGIC.ENV_UPDATE) {
          // magic(4) + json_len(4) + json(N) = 8 + N bytes
          if (rxAvailable() < 8) break;
          const jsonLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 8 + jsonLen) break;
          const jsonStr = compositorRxBuffer.slice(rxReadOffset + 8, rxReadOffset + 8 + jsonLen).toString('utf8');
          rxConsume(8 + jsonLen);

          try {
            const vars = JSON.parse(jsonStr) as Record<string, string>;
            for (const [key, value] of Object.entries(vars)) {
              process.env[key] = value;
              debugLog(`[Wo] env update: ${key}=${value}`);
            }
            if (mainWindow && !mainWindow.isDestroyed()) {
              mainWindow.webContents.send('wo:env-update', vars);
            }
          } catch (err) {
            debugLog('[Wo] ENV_UPDATE parse error:', err);
          }
          continue;
        }

        if (magic === MAGIC.SCREENCOPY_EVENT) {
          // magic(4) + active(4) + client_count(4) = 12 bytes
          if (rxAvailable() < 12) break;
          const active = compositorRxBuffer.readUInt32LE(rxReadOffset + 4) !== 0;
          const clientCount = compositorRxBuffer.readUInt32LE(rxReadOffset + 8);
          rxConsume(12);

          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send('wo:screencopy-event', { active, clientCount });
          }
          continue;
        }

        // Unknown message magic; resync by advancing one byte.
        debugLog('[Wo] UNKNOWN magic 0x' + magic.toString(16).padStart(8, '0') + ' bufLen=' + rxAvailable() + ' — stream may be corrupted');
        rxConsume(1);
      }
      const dataElapsed = Date.now() - dataT0;
      if (dataElapsed > 50) {
        console.warn(`[IPC SLOW] data handler took ${dataElapsed}ms chunkLen=${chunk.length}`);
      }
    });

    ipcSocket.on('error', (error: Error) => {
      debugLog('[Wo] IPC connection error:', error);
      const hadConnected = ipcConnected;
      ipcConnected = false;
      dmabufSender = null;
      if (hadConnected) {
        scheduleIpcReconnect();
        return;
      }
      reject(error);
    });
    ipcSocket.on('close', () => {
      debugLog('[Wo] IPC connection closed');
      ipcConnected = false;
      ipcSocket = null;
      dmabufSender = null;
      inFlightFrameSeqs.clear();
      compositorRxBuffer = Buffer.alloc(64 * 1024);
      rxWriteOffset = 0;
      rxReadOffset = 0;
      // Clean up surface caches
      surfaceSabCache.clear();
      surfaceUpdatePending.clear();
      closeShmFdCache();
      scheduleIpcReconnect();
    });

    setTimeout(() => {
      if (!ipcConnected) {
        debugLog('[Wo] IPC connection timeout');
        reject(new Error('Failed to connect to compositor IPC socket'));
      }
    }, 5000);
  });
}

function setupIpcHandlers() {
  ipcMain.handle('action', (_event, action: string, payload?: Record<string, unknown>) => {
    return sendActionToCompositor(action, payload);
  });

  ipcMain.on('action-sync', (_event, action: string, payload?: Record<string, unknown>) => {
    sendActionToCompositor(action, payload);
  });

  ipcMain.handle('wo:get-windows', async () => compositorWindows.length > 0 ? compositorWindows : clientWindows);

  ipcMain.on('wo:keybind-action', (_event, action: string) => {
    if (action === 'shuffle') {
      clientWindows = [...clientWindows].sort(() => Math.random() - 0.5);
    } else if (action === 'reverse') {
      clientWindows = [...clientWindows].reverse();
    }

    if (mainWindow) {
      mainWindow.webContents.send('wo:windows', clientWindows);
    }
  });

  ipcMain.handle('wo:submit-damage-frame', (_event, payload: DamageFramePayload) => {
    if (!dmabufSender) {
      return { ok: false, reason: 'dmabuf-sender-not-ready' };
    }

    if (inFlightFrameSeqs.size >= MAX_IN_FLIGHT_FRAMES) {
      return { ok: true, skipped: true, reason: 'backpressure' };
    }

    if (!damageBuffer) {
      damageBuffer = new DamageBuffer(payload.width, payload.height);
    }

    // Mark that the app is using the damage helper path; OSR paint will be suppressed
    appUsedDamageHelper = true;
    applyDamagePayload(damageBuffer, payload);
    const seq = dmabufSender.send(damageBuffer);
    inFlightFrameSeqs.add(seq.toString());
    return { ok: true, seq: seq.toString() };
  });

  // Keyboard forwarding: renderer sends evdev key events for focused Wayland canvas windows
  ipcMain.on('wo:forward-keyboard', (_event, windowName: string, key: number, pressed: boolean, time: number) => {
    if (ipcSocket && ipcConnected) {
      const nameBuf = Buffer.from(windowName, 'utf8');
      const msg = Buffer.alloc(4 + 4 + nameBuf.length + 12);
      let off = 0;
      msg.writeUInt32LE(MAGIC.FORWARD_KEYBOARD, off); off += 4;
      msg.writeUInt32LE(nameBuf.length, off); off += 4;
      nameBuf.copy(msg, off); off += nameBuf.length;
      msg.writeUInt32LE(key, off); off += 4;
      msg.writeUInt32LE(pressed ? 1 : 0, off); off += 4;
      msg.writeUInt32LE(time || 0, off);
      ipcSocket.write(msg);
    }
  });

  ipcMain.on('wo:forward-relative-pointer', (_event, windowName: string, dx: number, dy: number) => {
    if (ipcSocket && ipcConnected) {
      const nameBuf = Buffer.from(windowName, 'utf8');
      const msg = Buffer.alloc(4 + 4 + nameBuf.length + 16);
      let off = 0;
      msg.writeUInt32LE(MAGIC.FORWARD_RELATIVE_POINTER, off); off += 4;
      msg.writeUInt32LE(nameBuf.length, off); off += 4;
      nameBuf.copy(msg, off); off += nameBuf.length;
      msg.writeDoubleLE(dx, off); off += 8;
      msg.writeDoubleLE(dy, off);
      ipcSocket.write(msg);
    }
  });

  // Syscall handler for system operations
  const iconCache = new Map<string, { type: string; data: string; mimeType?: string } | null>();

  function resolveDesktopIconFast(iconName: string, fs: any, path: any): { type: string; data: string; mimeType?: string } | null {
    if (iconCache.has(iconName)) return iconCache.get(iconName) ?? null;

    let result: { type: string; data: string; mimeType?: string } | null = null;

    // Absolute path: read directly
    if (iconName.startsWith('/')) {
      try {
        if (fs.existsSync(iconName)) {
          const ext = path.extname(iconName).toLowerCase();
          const mime = ext === '.svg' ? 'image/svg+xml' : 'image/png';
          result = { type: 'base64', data: fs.readFileSync(iconName).toString('base64'), mimeType: mime };
        }
      } catch { /* ignore */ }
      iconCache.set(iconName, result);
      return result;
    }

    // Only check 4 fast paths to avoid blocking the event loop:
    // 1. /usr/share/pixmaps/{name}.png
    // 2. /usr/share/pixmaps/{name}.svg
    // 3. hicolor 48x48/apps/{name}.png
    // 4. hicolor scalable/apps/{name}.svg
    const quickPaths = [
      [`/usr/share/pixmaps/${iconName}.png`, 'image/png'],
      [`/usr/share/pixmaps/${iconName}.svg`, 'image/svg+xml'],
      [`/usr/share/icons/hicolor/48x48/apps/${iconName}.png`, 'image/png'],
      [`/usr/share/icons/hicolor/scalable/apps/${iconName}.svg`, 'image/svg+xml'],
    ];

    for (const [iconPath, mime] of quickPaths) {
      try {
        if (fs.existsSync(iconPath)) {
          result = { type: 'base64', data: fs.readFileSync(iconPath).toString('base64'), mimeType: mime };
          break;
        }
      } catch { /* ignore */ }
    }

    iconCache.set(iconName, result);
    return result;
  }

  ipcMain.handle('wo:syscall', async (_event, type: string, params: Record<string, unknown>) => {
    try {
      switch (type) {
        case 'list_applications': {
          // Scan desktop files from standard locations
          const path = await import('path');
          const fs = await import('fs');

          const apps: Array<{ name: string; icon?: { type: string; data: string }; command: string }> = [];
          const dirs = ['/usr/share/applications', `${process.env.HOME}/.local/share/applications`];
          const seen = new Set<string>();

          for (const dir of dirs) {
            try {
              if (!fs.default.existsSync(dir)) continue;
              const files = fs.default.readdirSync(dir).filter((f: string) => f.endsWith('.desktop'));

              for (const file of files) {
                const filePath = path.default.join(dir, file);
                try {
                  const content = fs.default.readFileSync(filePath, 'utf-8');
                  if (content.includes('NoDisplay=true')) continue;

                  const nameMatch = content.match(/^Name=(.+)$/m);
                  const execMatch = content.match(/^Exec=(.+)$/m);
                  const iconMatch = content.match(/^Icon=(.+)$/m);

                  if (nameMatch && execMatch) {
                    const name = nameMatch[1].trim();
                    if (!seen.has(name)) {
                      seen.add(name);
                      const exec = execMatch[1].trim().split(' ')[0].split('/').pop() || execMatch[1].trim();
                      const app: any = {
                        name,
                        command: exec,
                      };
                      // Resolve icon: if absolute path use it directly, otherwise search icon theme dirs
                      if (iconMatch) {
                        const iconName = iconMatch[1].trim();
                        const resolved = resolveDesktopIconFast(iconName, fs.default, path.default);
                        if (resolved) {
                          app.icon = resolved;
                        } else {
                          app.icon = { type: 'iconify', data: iconName };
                        }
                      }
                      apps.push(app);
                    }
                  }
                } catch {
                  // Skip files that can't be read
                }
              }
            } catch {
              // Skip directories that don't exist or can't be read
            }
          }

          return apps.length > 0 ? apps : [
            { name: 'Terminal', icon: { type: 'iconify', data: 'mdi:console' }, command: 'xterm' },
            { name: 'Firefox', icon: { type: 'iconify', data: 'mdi:firefox' }, command: 'firefox' },
            { name: 'Files', icon: { type: 'iconify', data: 'mdi:folder' }, command: 'nautilus' },
          ];
        }

        case 'launch':
        case 'exec': {
          const { spawn } = await import('child_process');
          const command = (params.command as string) || '';

          if (!command) {
            return { ok: false, error: 'No command specified' };
          }

          try {
            const child = spawn(command, {
              detached: true,
              stdio: 'ignore',
              shell: true,
            });
            child.unref();
            return { ok: true, pid: child.pid };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }

        case 'listdir': {
          const fs = await import('fs');
          const path = await import('path');
          const dirPath = (params.path as string) || process.env.HOME || '/home';

          try {
            const entries = fs.default.readdirSync(dirPath, { withFileTypes: true });
            return {
              ok: true,
              path: dirPath,
              entries: entries.map((e) => ({
                name: e.name,
                type: e.isDirectory() ? 'dir' : 'file',
              })),
            };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }

        case 'read': {
          const fs = await import('fs');
          const filePath = params.path as string;

          if (!filePath) {
            return { ok: false, error: 'No file path specified' };
          }

          try {
            const content = fs.default.readFileSync(filePath, 'utf-8');
            return { ok: true, content, size: content.length };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }

        case 'write': {
          const fs = await import('fs');
          const filePath = params.path as string;
          const content = params.content as string;

          if (!filePath || content === undefined) {
            return { ok: false, error: 'Missing file path or content' };
          }

          try {
            fs.default.writeFileSync(filePath, content);
            return { ok: true, bytesWritten: content.length };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }

        case 'shutdown': {
          const { spawn } = await import('child_process');
          spawn('systemctl', ['poweroff'], { detached: true, stdio: 'ignore' }).unref();
          return { ok: true };
        }

        case 'restart': {
          const { spawn } = await import('child_process');
          spawn('systemctl', ['reboot'], { detached: true, stdio: 'ignore' }).unref();
          return { ok: true };
        }

        case 'logout': {
          const { spawn } = await import('child_process');
          const sessionId = process.env.XDG_SESSION_ID || '';
          if (sessionId) {
            spawn('loginctl', ['terminate-session', sessionId], { detached: true, stdio: 'ignore' }).unref();
          } else {
            // Fallback: quit the compositor
            sendActionToCompositor('quit', { code: 0 });
          }
          return { ok: true };
        }

        case 'lock': {
          const { spawn } = await import('child_process');
          spawn('loginctl', ['lock-session'], { detached: true, stdio: 'ignore' }).unref();
          return { ok: true };
        }

        case 'sleep': {
          const { spawn } = await import('child_process');
          spawn('systemctl', ['suspend'], { detached: true, stdio: 'ignore' }).unref();
          return { ok: true };
        }

        case 'notify': {
          // Forward notification to renderer
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send('wo:notification', {
              id: params.id || `notify-${Date.now()}`,
              title: params.title || 'Notification',
              body: params.body || '',
              icon: params.icon,
              timeout: params.timeout ?? 5000,
              timestamp: Date.now(),
            });
          }
          return { ok: true };
        }

        case 'tray_list': {
          return getStatusNotifierTrayItems();
        }

        case 'tray_activate': {
          const service = String(params.service || '');
          const objectPath = String(params.objectPath || '');
          if (!service || !objectPath) {
            return { ok: false, error: 'Missing service or objectPath' };
          }
          const ok = activateStatusNotifierItem(service, objectPath);
          return ok ? { ok: true } : { ok: false, error: 'Activation failed' };
        }

        case 'tray_secondary_activate': {
          const service = String(params.service || '');
          const objectPath = String(params.objectPath || '');
          if (!service || !objectPath) {
            return { ok: false, error: 'Missing service or objectPath' };
          }
          const ok = secondaryActivateStatusNotifierItem(service, objectPath);
          return ok ? { ok: true } : { ok: false, error: 'Secondary activation failed' };
        }

        case 'tray_context_menu': {
          const service = String(params.service || '');
          const objectPath = String(params.objectPath || '');
          const x = Number(params.x ?? 0);
          const y = Number(params.y ?? 0);
          if (!service || !objectPath) {
            return { ok: false, error: 'Missing service or objectPath' };
          }
          const ok = openStatusNotifierContextMenu(service, objectPath, x, y);
          return ok ? { ok: true } : { ok: false, error: 'Context menu failed' };
        }

        case 'tray_menu_items': {
          const service = String(params.service || '');
          const menuPath = String(params.menuPath || '');
          if (!service || !menuPath) {
            return [];
          }
          return getDBusMenuLayout(service, menuPath);
        }

        case 'tray_menu_event': {
          const service = String(params.service || '');
          const menuPath = String(params.menuPath || '');
          const menuId = String(params.menuId || '');
          if (!service || !menuPath || !menuId) {
            return { ok: false, error: 'Missing parameters' };
          }
          const ok = triggerDBusMenuEvent(service, menuPath, menuId);
          return ok ? { ok: true } : { ok: false, error: 'Menu event failed' };
        }

        case 'portal_respond': {
          const requestId = String(params.requestId || '');
          if (!requestId) {
            return { ok: false, error: 'Missing portal requestId' };
          }
          return respondToPortalRequest(requestId, {
            allowed: Boolean(params.allowed),
            type: typeof params.type === 'string' ? params.type : undefined,
            windowName: params.windowName,
          });
        }

        default:
          return { ok: false, error: `Unknown syscall: ${type}` };
      }
    } catch (error) {
      return { ok: false, error: `Syscall error: ${String(error)}` };
    }
  });
}

async function createWindow() {
  debugLog('[Wo] createWindow starting');
  const preloadPath = joinPath(__dirname, 'preload.js');
  debugLog('[Wo] preloadPath=' + preloadPath);

  try {
    mainWindow = new BrowserWindow({
      width: CLIENT_MODE ? 1400 : width,
      height: CLIENT_MODE ? 900 : height,
      transparent: true,
      backgroundColor: '#00000000',
      frame: CLIENT_MODE,
      webPreferences: {
        sandbox: false,
        contextIsolation: true,
        preload: preloadPath,
        nodeIntegration: false,
        offscreen: !CLIENT_MODE,
        webgl: true,
        backgroundThrottling: false,
      },
      show: CLIENT_MODE,
    });

    // Enable SharedArrayBuffer in the renderer by injecting
    // cross-origin isolation headers on all responses.
    mainWindow.webContents.session.webRequest.onHeadersReceived((details, callback) => {
      callback({
        responseHeaders: {
          ...details.responseHeaders,
          'Cross-Origin-Opener-Policy': ['same-origin'],
          'Cross-Origin-Embedder-Policy': ['require-corp'],
        },
      });
    });

    debugLog('[Wo] BrowserWindow created, CLIENT_MODE=' + CLIENT_MODE + ' offscreen=' + !CLIENT_MODE);

    setupIpcHandlers();
    debugLog('[Wo] setupIpcHandlers completed');

    // Send initial window list regardless of mode
    mainWindow.webContents.send('wo:windows', clientWindows);

    if (CLIENT_MODE) {
      return;
    }

    // In offscreen mode, we need actual content to trigger paint events
    // Load the configured URL or fallback to minimal page
    debugLog('[Wo] Loading content URL: ' + (url || 'minimal fallback'));
    try {
      const pageUrl = url || 'data:text/html,<!DOCTYPE html><html><head><style>body{background:white;margin:0;}</style></head><body></body></html>';
      const loadPromise = mainWindow!.loadURL(pageUrl);
      // Give it time to load but don't wait forever
      await Promise.race([
        loadPromise,
        new Promise(r => setTimeout(() => { debugLog('[Wo] Load timeout, continuing'); r(undefined); }, 3000))
      ]);
      debugLog('[Wo] Content loaded from: ' + pageUrl);
    } catch (e) {
      debugLog('[Wo] Load failed:', String(e));
    }

    debugLog('[Wo] Page initialized, setting up rendering');

    // Ensure the webcontents has focus so sendInputEvent works for keyboard events
    mainWindow.webContents.focus();

    if (!CLIENT_MODE) {
      debugLog('[Wo] Setting up OSR rendering, FPS=' + (WINDOW_CONFIG.fps ?? 60));
      mainWindow.webContents.setFrameRate(WINDOW_CONFIG.fps ?? 60);
      mainWindow.webContents.on('did-finish-load', () => {
        mainWindow!.webContents.setFrameRate(WINDOW_CONFIG.fps ?? 60);
      });

      // Invalidate periodically to ensure continuous repaints for dynamic
      // content.  We only invalidate when below the in-flight limit so we
      // don't pile up redundant paints while the compositor is still
      // processing previous frames.
      const frameIntervalMs = Math.round(1000 / (WINDOW_CONFIG.fps ?? 60));
      setInterval(() => {
        if (mainWindow && !mainWindow.isDestroyed() && inFlightFrameSeqs.size < MAX_IN_FLIGHT_FRAMES) {
          mainWindow.webContents.invalidate();
        }
      }, frameIntervalMs);

      // Reusable patch buffer to avoid allocating a new typed array on
      // every partial-update paint event.
      let patchBuf: Uint8Array | null = null;

      let skippedCount = 0;
      mainWindow.webContents.on('paint', (_event, dirty, image) => {
        if (!dmabufSender || !ipcConnected) {
          skippedCount++;
          debugLog('[Wo] SKIPPED paint (dmabufSender=' + !!dmabufSender + ' ipcConnected=' + ipcConnected + ')');
          return;
        }
        if (appUsedDamageHelper) {
          return;
        }
        if (inFlightFrameSeqs.size >= MAX_IN_FLIGHT_FRAMES) {
          skippedCount++;
          if (skippedCount === 1 || skippedCount % 120 === 0) {
            debugLog('[Wo] SKIPPED paint due to frame backpressure inFlight=' + inFlightFrameSeqs.size);
          }
          return;
        }
        const size = image.getSize();
        if (!damageBuffer) {
          damageBuffer = new DamageBuffer(size.width, size.height);
        }
        const pixels = image.toBitmap();
        const pixelData = new Uint8Array(pixels.buffer, pixels.byteOffset, pixels.byteLength);

        // Use dirty rect to avoid full-frame copy when only a region changed
        const isFullFrame = dirty.x === 0 && dirty.y === 0
          && dirty.width === size.width && dirty.height === size.height;

        if (isFullFrame) {
          applyDamagePayload(damageBuffer, {
            width: size.width,
            height: size.height,
            fullFrame: pixelData,
          });
        } else {
          // Partial update — copy only the dirty region into the damage
          // buffer.  Reuse a single scratch buffer to avoid per-frame GC
          // pressure from allocating a new Uint8Array every paint.
          const stride = size.width * 4;
          const patchStride = dirty.width * 4;
          const patchBytes = patchStride * dirty.height;
          if (!patchBuf || patchBuf.byteLength < patchBytes) {
            patchBuf = new Uint8Array(patchBytes);
          }
          const patchData = patchBuf.byteLength === patchBytes
            ? patchBuf
            : patchBuf.subarray(0, patchBytes);
          for (let row = 0; row < dirty.height; row++) {
            const srcOff = (dirty.y + row) * stride + dirty.x * 4;
            const dstOff = row * patchStride;
            patchData.set(pixelData.subarray(srcOff, srcOff + patchStride), dstOff);
          }
          applyDamagePayload(damageBuffer, {
            width: size.width,
            height: size.height,
            patches: [{
              rect: { x: dirty.x, y: dirty.y, width: dirty.width, height: dirty.height },
              rgba: patchData,
              stride: patchStride,
            }],
          });
        }

        try {
          const seq = dmabufSender.send(damageBuffer);
          inFlightFrameSeqs.add(seq.toString());
        } catch (e) {
          debugLog('[Wo] ERROR sending frame:', String(e));
        }
      });
    }

    if (process.env.WO_DEBUG === '1') {
      mainWindow.webContents.openDevTools();
    }

    debugLog('[Wo] createWindow completed successfully');
  } catch (error) {
    debugLog('[Wo] createWindow error:', error);
    throw error;
  }

  mainWindow.on('closed', () => {
    mainWindow = null;
    if (windowPositionUpdateTimer) {
      clearInterval(windowPositionUpdateTimer);
      windowPositionUpdateTimer = null;
    }
  });

  // Send window position updates to compositor for window capture and positioning
  if (!CLIENT_MODE && ipcConnected) {
    const sendPosition = () => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        const bounds = mainWindow.getBounds();
        sendWindowPositionUpdate(bounds.x, bounds.y, bounds.width, bounds.height);
      }
    };

    // Send initial position
    sendPosition();

    // Update position periodically (every 100ms)
    windowPositionUpdateTimer = setInterval(sendPosition, 100);

    // Also send on move/resize events
    mainWindow.on('move', sendPosition);
    mainWindow.on('resize', sendPosition);
  }
}

app.on('ready', async () => {
  debugLog('[Wo] App ready, CLIENT_MODE=', CLIENT_MODE, 'WO_STANDALONE=', process.env.WO_STANDALONE);

  // Log GPU status so we can verify hardware acceleration is active.
  const gpuInfo = app.getGPUFeatureStatus();
  debugLog('[Wo] GPU feature status:', JSON.stringify(gpuInfo));
  debugLog('[Wo] GPU info:', JSON.stringify(app.getGPUInfo('basic').catch(() => ({}))));

  try {
    startPortalUiBridge();

    if (!CLIENT_MODE && process.env.WO_STANDALONE !== '1') {
      debugLog('[Wo] Will connect to compositor');
      await connectToCompositor();
    }

    debugLog('[Wo] Creating window');
    await createWindow();
    debugLog('[Wo] Window created successfully');
  } catch (error) {
    debugLog('[Wo] Failed to start app:', error);
    app.quit();
  }
});

app.on('window-all-closed', () => {
  stopPortalUiBridge();
  if (process.platform !== 'darwin') {
    app.quit();
  }
});

app.on('activate', () => {
  if (mainWindow === null) {
    createWindow().catch(console.error);
  }
});

app.on('before-quit', () => {
  stopPortalUiBridge();
  closeShmFdCache();
});

export { sendActionToCompositor, connectToCompositor };
