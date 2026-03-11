import { createRequire } from "node:module";
var __create = Object.create;
var __getProtoOf = Object.getPrototypeOf;
var __defProp = Object.defineProperty;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __toESM = (mod, isNodeMode, target) => {
  target = mod != null ? __create(__getProtoOf(mod)) : {};
  const to = isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target;
  for (let key of __getOwnPropNames(mod))
    if (!__hasOwnProp.call(to, key))
      __defProp(to, key, {
        get: () => mod[key],
        enumerable: true
      });
  return to;
};
var __require = /* @__PURE__ */ createRequire(import.meta.url);

// src/main.ts
import { app, BrowserWindow, ipcMain } from "electron";
import { createConnection, createServer } from "net";
import { fileURLToPath } from "url";
import { dirname, resolve as resolvePath, join as joinPath } from "path";
import fs from "fs";
import { createRequire as createRequire2 } from "module";

// src/protocol.ts
var MAGIC = {
  HELLO: 1464813644,
  FRAME: 1464813138,
  MOUSE_MOVE: 1464814925,
  MOUSE_BUTTON: 1464814914,
  KEYBOARD: 1464814402,
  SCROLL: 1464816451,
  ACTION: 1464812353,
  FOCUS_CHANGE: 1464813123,
  WINDOW_META: 1464817485,
  WINDOW_POS: 1464817488,
  SYSCALL: 1464816473,
  FRAME_ACK: 1464813121,
  SURFACE_BUFFER: 1464816450,
  SHM_BUFFER: 1464816461,
  DMABUF_FRAME: 1464812614,
  FORWARD_POINTER: 1464815685,
  FORWARD_KEYBOARD: 1464814405,
  FORWARD_RELATIVE_POINTER: 1464816197,
  POINTER_LOCK_REQUEST: 1464815692,
  ENV_UPDATE: 1464812885,
  SCREENCOPY_EVENT: 1464816453
};
function stringToBuffer(str) {
  return Buffer.from(str, "utf-8");
}

// src/damage-buffer.ts
import { writeSync } from "fs";
var ARGB8888 = fourccCode("A", "R", "2", "4");

class DamageBuffer {
  width;
  height;
  stride;
  frame;
  constructor(width, height, stride = width * 4) {
    this.width = width;
    this.height = height;
    this.stride = stride;
    this.frame = Buffer.alloc(this.stride * this.height);
  }
  getWidth() {
    return this.width;
  }
  getHeight() {
    return this.height;
  }
  getStride() {
    return this.stride;
  }
  getFrameBuffer() {
    return this.frame;
  }
  reset(width, height, stride = width * 4) {
    this.width = width;
    this.height = height;
    this.stride = stride;
    this.frame = Buffer.alloc(this.stride * this.height);
  }
  applyFullFrame(frame, frameStride = this.width * 4) {
    if (frameStride === this.stride) {
      this.frame.set(new Uint8Array(frame.buffer, frame.byteOffset, this.frame.length));
      return;
    }
    const rowBytes = Math.min(frameStride, this.stride);
    for (let y = 0;y < this.height; y += 1) {
      const srcStart = y * frameStride;
      const dstStart = y * this.stride;
      this.frame.set(new Uint8Array(frame.buffer, frame.byteOffset + srcStart, rowBytes), dstStart);
    }
  }
  applyPatch(patch) {
    const rect = clampRect(patch.rect, this.width, this.height);
    if (rect.width <= 0 || rect.height <= 0) {
      return;
    }
    const patchStride = patch.stride ?? rect.width * 4;
    const dstStartX = rect.x * 4;
    const rowBytes = rect.width * 4;
    for (let row = 0;row < rect.height; row += 1) {
      const srcStart = row * patchStride;
      const dstStart = (rect.y + row) * this.stride + dstStartX;
      this.frame.set(new Uint8Array(patch.rgba.buffer, patch.rgba.byteOffset + srcStart, rowBytes), dstStart);
    }
  }
  applyPatches(patches) {
    for (const patch of patches) {
      this.applyPatch(patch);
    }
  }
}

class WoDmabufSender {
  socket;
  windowName;
  native;
  seq = 1n;
  wireBuf;
  nameLen;
  seqOffset;
  widthOffset;
  heightOffset;
  constructor(socket, windowName, native) {
    this.socket = socket;
    this.windowName = windowName;
    this.native = native;
    const nameBuf = Buffer.from(windowName, "utf8");
    this.nameLen = nameBuf.length;
    const totalLen = 32 + this.nameLen + 16;
    this.wireBuf = Buffer.alloc(totalLen);
    let off = 0;
    this.wireBuf.writeUInt32LE(MAGIC.FRAME, off);
    off += 4;
    this.wireBuf.writeUInt32LE(this.nameLen, off);
    off += 4;
    nameBuf.copy(this.wireBuf, off);
    off += this.nameLen;
    this.seqOffset = off;
    off += 8;
    this.widthOffset = off;
    off += 4;
    this.heightOffset = off;
    off += 4;
    this.wireBuf.writeUInt32LE(ARGB8888, off);
    off += 4;
    this.wireBuf.writeUInt32LE(1, off);
  }
  send(buffer) {
    const imported = this.native.importRgba(buffer.getFrameBuffer(), buffer.getWidth(), buffer.getHeight(), buffer.getStride());
    try {
      const socketFd = getSocketFd(this.socket);
      this.wireBuf.writeBigUInt64LE(this.seq, this.seqOffset);
      this.wireBuf.writeUInt32LE(buffer.getWidth(), this.widthOffset);
      this.wireBuf.writeUInt32LE(buffer.getHeight(), this.heightOffset);
      const planeOff = this.wireBuf.length - 16;
      this.wireBuf.writeUInt32LE(imported.offset, planeOff);
      this.wireBuf.writeUInt32LE(imported.stride, planeOff + 4);
      this.wireBuf.writeUInt32LE(imported.modifier_hi, planeOff + 8);
      this.wireBuf.writeUInt32LE(imported.modifier_lo, planeOff + 12);
      writeSync(socketFd, this.wireBuf, 0, this.wireBuf.length);
      this.native.sendFd(socketFd, imported.fd);
      const sentSeq = this.seq;
      this.seq += 1n;
      return sentSeq;
    } finally {
      this.native.releaseBuffer(imported.token);
    }
  }
}
function applyDamagePayload(buffer, payload) {
  if (payload.width !== buffer.getWidth() || payload.height !== buffer.getHeight()) {
    buffer.reset(payload.width, payload.height);
  }
  if (payload.fullFrame) {
    buffer.applyFullFrame(payload.fullFrame, payload.fullStride ?? payload.width * 4);
  }
  if (payload.patches && payload.patches.length > 0) {
    buffer.applyPatches(payload.patches);
  }
}
function loadNativeDmabufModule(requireFn, nativePath) {
  const native = requireFn(nativePath);
  return native;
}
function getSocketFd(socket) {
  const fd = socket?._handle?.fd;
  if (typeof fd !== "number" || !Number.isFinite(fd) || fd < 0) {
    throw new Error("Unable to read underlying socket file descriptor");
  }
  return fd;
}
function clampRect(rect, width, height) {
  const x = Math.max(0, Math.min(width, rect.x));
  const y = Math.max(0, Math.min(height, rect.y));
  const right = Math.max(x, Math.min(width, rect.x + rect.width));
  const bottom = Math.max(y, Math.min(height, rect.y + rect.height));
  return { x, y, width: Math.max(0, right - x), height: Math.max(0, bottom - y) };
}
function fourccCode(a, b, c, d) {
  return (a.charCodeAt(0) | b.charCodeAt(0) << 8 | c.charCodeAt(0) << 16 | d.charCodeAt(0) << 24) >>> 0;
}

// src/main.ts
var __filename2 = fileURLToPath(import.meta.url);
var __dirname2 = dirname(__filename2);
var require2 = createRequire2(import.meta.url);
var DEBUG_LOG = "/tmp/wo-electron-debug.log";
var _logBuffer = [];
var _logTimer = null;
function _flushLog() {
  if (_logBuffer.length === 0)
    return;
  try {
    fs.appendFileSync(DEBUG_LOG, _logBuffer.join(""));
  } catch (e) {}
  _logBuffer = [];
  _logTimer = null;
}
function debugLog(...args) {
  const msg = `[${new Date().toISOString()}] ` + args.join(" ") + `
`;
  _logBuffer.push(msg);
  if (!_logTimer) {
    _logTimer = setTimeout(_flushLog, 100);
  }
  console.log(...args);
}
debugLog("[Wo] Electron starting, NODE_VERSION=", process.version);
app.commandLine.appendSwitch("use-gl", "angle");
app.commandLine.appendSwitch("use-angle", "vulkan");
app.commandLine.appendSwitch("ozone-platform", "wayland");
app.commandLine.appendSwitch("no-sandbox");
app.commandLine.appendSwitch("disable-gpu-sandbox");
app.commandLine.appendSwitch("ignore-gpu-blocklist");
app.commandLine.appendSwitch("enable-gpu-rasterization");
app.commandLine.appendSwitch("enable-zero-copy");
app.commandLine.appendSwitch("enable-native-gpu-memory-buffers");
app.commandLine.appendSwitch("disable-software-rasterizer");
app.commandLine.appendSwitch("enable-features", "CanvasOopRasterization,Vulkan,VaapiVideoDecoder,VaapiVideoEncoder,SharedArrayBuffer,RawDraw,DefaultANGLEVulkan,VulkanFromANGLE");
app.commandLine.appendSwitch("enable-accelerated-video-decode");
app.commandLine.appendSwitch("disable-frame-rate-limit");
app.commandLine.appendSwitch("disable-gpu-vsync");
app.commandLine.appendSwitch("num-raster-threads", "4");
app.commandLine.appendSwitch("disable-renderer-backgrounding");
debugLog("[Wo] GPU acceleration flags applied");
var IPC_SOCKET = process.env.WO_IPC_SOCKET || "/run/user/1000/wo-ipc.sock";
debugLog("[Wo] WO_WINDOW_CONFIG env =", process.env.WO_WINDOW_CONFIG);
var WINDOW_CONFIG = JSON.parse(process.env.WO_WINDOW_CONFIG || "{}");
debugLog("[Wo] WINDOW_CONFIG parsed =", JSON.stringify(WINDOW_CONFIG));
var CLIENT_MODE = process.env.WO_CLIENT_MODE === "1";
var {
  name = "default",
  url = null,
  html = null,
  width = 1920,
  height = 1080
} = WINDOW_CONFIG;
var mainWindow = null;
var ipcSocket = null;
var portalUiServer = null;
var portalPendingRequests = new Map;
var PORTAL_UI_SOCKET = "/tmp/wo-portal-ui.sock";
var ipcConnected = false;
var ipcReconnectTimer = null;
var windowPositionUpdateTimer = null;
var dmabufSender = null;
var damageBuffer = null;
var nativeDmabuf = null;
var compositorRxBuffer = Buffer.alloc(64 * 1024);
var rxWriteOffset = 0;
var rxReadOffset = 0;
var surfaceSabCache = new Map;
var surfaceUpdatePending = new Map;
var surfaceUpdateGeneration = 0;
var surfaceUpdateFlushScheduled = false;
var shmFdCache = new Map;
function rxAppend(chunk) {
  const needed = rxWriteOffset + chunk.length;
  if (needed > compositorRxBuffer.length) {
    if (rxReadOffset > 0) {
      compositorRxBuffer.copy(compositorRxBuffer, 0, rxReadOffset, rxWriteOffset);
      rxWriteOffset -= rxReadOffset;
      rxReadOffset = 0;
    }
    if (rxWriteOffset + chunk.length > compositorRxBuffer.length) {
      const next = Buffer.alloc(Math.max(compositorRxBuffer.length * 2, rxWriteOffset + chunk.length));
      compositorRxBuffer.copy(next, 0, 0, rxWriteOffset);
      compositorRxBuffer = next;
    }
  }
  chunk.copy(compositorRxBuffer, rxWriteOffset);
  rxWriteOffset += chunk.length;
}
function rxAvailable() {
  return rxWriteOffset - rxReadOffset;
}
function rxConsume(n) {
  rxReadOffset += n;
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
    } catch {}
  }
  shmFdCache.clear();
}
function getOrOpenShmFd(windowName, pid, fd) {
  const cached = shmFdCache.get(windowName);
  if (cached && cached.pid === pid && cached.fd === fd) {
    return cached.extFd;
  }
  if (cached) {
    try {
      fs.closeSync(cached.extFd);
    } catch {}
  }
  const fdPath = `/proc/${pid}/fd/${fd}`;
  const extFd = fs.openSync(fdPath, "r");
  shmFdCache.set(windowName, { pid, fd, extFd });
  return extFd;
}
function getOrCreateSab(windowName, width2, height2, stride) {
  const existing = surfaceSabCache.get(windowName);
  const neededSize = stride * height2;
  if (existing && existing.sab.byteLength >= neededSize && existing.width === width2 && existing.height === height2 && existing.stride === stride) {
    return existing;
  }
  const sab = new SharedArrayBuffer(neededSize);
  const entry = { sab, width: width2, height: height2, stride };
  surfaceSabCache.set(windowName, entry);
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.postMessage("wo:surface-sab", {
      name: windowName,
      sab,
      width: width2,
      height: height2,
      stride
    });
  }
  return entry;
}
function flushSurfaceUpdates() {
  surfaceUpdateFlushScheduled = false;
  if (!mainWindow || mainWindow.isDestroyed()) {
    surfaceUpdatePending.clear();
    return;
  }
  for (const entry of surfaceUpdatePending.values()) {
    mainWindow.webContents.send("wo:surface-update", entry);
  }
  surfaceUpdatePending.clear();
}
var inFlightFrameSeqs = new Set;
var MAX_IN_FLIGHT_FRAMES = 3;
var appUsedDamageHelper = false;
var pointerX = 0;
var pointerY = 0;
function respondToPortalRequest(requestId, params) {
  const pending = portalPendingRequests.get(requestId);
  if (!pending) {
    return { ok: false, error: `No pending portal request: ${requestId}` };
  }
  let response;
  if (pending.kind === "screen_share") {
    response = {
      allowed: Boolean(params.allowed),
      sourceType: params.type === "window" ? "Window" : "Monitor",
      windowName: typeof params.windowName === "string" ? params.windowName : null
    };
  } else {
    response = { allowed: false };
  }
  try {
    pending.socket.write(`${JSON.stringify(response)}
`);
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
  } catch {}
  portalUiServer = createServer((socket) => {
    let received = "";
    socket.on("data", (chunk) => {
      received += chunk.toString("utf8");
      const nl = received.indexOf(`
`);
      if (nl < 0)
        return;
      const line = received.slice(0, nl).trim();
      received = received.slice(nl + 1);
      if (!line)
        return;
      try {
        const req = JSON.parse(line);
        if (req.type !== "screen_share_request" || !req.sessionId) {
          socket.end(`{"allowed":false,"reason":"invalid_request"}
`);
          return;
        }
        const requestId = req.sessionId;
        portalPendingRequests.set(requestId, {
          socket,
          kind: "screen_share",
          sessionId: req.sessionId
        });
        socket.setTimeout(90000, () => {
          const pending = portalPendingRequests.get(requestId);
          if (pending) {
            portalPendingRequests.delete(requestId);
            pending.socket.end(`{"allowed":false,"reason":"timeout"}
`);
          }
        });
        if (mainWindow && !mainWindow.isDestroyed()) {
          const payload = {
            requestId,
            kind: "screen_share",
            appName: req.appName || "Application",
            sessionId: req.sessionId
          };
          mainWindow.webContents.send("wo:portal-request", payload);
        } else {
          portalPendingRequests.delete(requestId);
          socket.end(`{"allowed":false,"reason":"no_ui"}
`);
        }
      } catch {
        socket.end(`{"allowed":false,"reason":"invalid_json"}
`);
      }
    });
    socket.on("close", () => {
      for (const [sessionId, pending] of portalPendingRequests.entries()) {
        if (pending.socket === socket) {
          portalPendingRequests.delete(sessionId);
          break;
        }
      }
    });
    socket.on("error", () => {});
  });
  portalUiServer.listen(PORTAL_UI_SOCKET, () => {
    debugLog("[Wo] Portal UI bridge listening at", PORTAL_UI_SOCKET);
  });
  portalUiServer.on("error", (err) => {
    console.error("[Wo] Portal UI bridge error:", err);
  });
}
function stopPortalUiBridge() {
  for (const pending of portalPendingRequests.values()) {
    try {
      pending.socket.end(`{"allowed":false,"reason":"shutdown"}
`);
    } catch {}
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
  } catch {}
}
function parseClientWindows() {
  try {
    const raw = process.env.WO_CLIENT_WINDOWS;
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed) && parsed.length > 0) {
        return parsed.map((w, idx) => ({
          id: String(w.id ?? w.name ?? `win-${idx + 1}`),
          title: String(w.title ?? w.name ?? `Window ${idx + 1}`),
          width: Number(w.width ?? 960),
          height: Number(w.height ?? 540)
        }));
      }
    }
  } catch (error) {
    console.warn("[Wo client] invalid WO_CLIENT_WINDOWS:", error);
  }
  return [{ id: String(name), title: String(name), width: Number(width), height: Number(height) }];
}
var clientWindows = parseClientWindows();
var compositorWindows = [];
function parseQuotedStrings(output) {
  const matches = output.matchAll(/"((?:[^"\\]|\\.)*)"/g);
  const values = [];
  for (const match of matches) {
    const raw = match[1];
    values.push(raw.replace(/\\"/g, '"').replace(/\\\\/g, "\\"));
  }
  return values;
}
function runBusctlUser(args) {
  try {
    const { execFileSync } = require2("child_process");
    return execFileSync("busctl", ["--user", ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"]
    }).trim();
  } catch {
    return "";
  }
}
function parseServiceAndPath(itemRef) {
  const slashIdx = itemRef.indexOf("/");
  if (slashIdx > 0) {
    return {
      service: itemRef.slice(0, slashIdx),
      objectPath: itemRef.slice(slashIdx)
    };
  }
  if (!itemRef) {
    return null;
  }
  return {
    service: itemRef,
    objectPath: "/StatusNotifierItem"
  };
}
function parseBusctlStringValue(output) {
  const quoted = parseQuotedStrings(output);
  if (quoted.length > 0) {
    return quoted[0];
  }
  const parts = output.split(/\s+/);
  return parts.length >= 2 ? parts.slice(1).join(" ").trim() : "";
}
function getBusProperty(service, objectPath, iface, prop) {
  const out = runBusctlUser(["get-property", service, objectPath, iface, prop]);
  return out ? parseBusctlStringValue(out) : "";
}
function mapTrayIcon(iconName, title) {
  const n = iconName.toLowerCase();
  const t = title.toLowerCase();
  if (n.includes("network") || t.includes("network") || t.includes("wifi"))
    return "mdi:wifi";
  if (n.includes("audio") || n.includes("volume") || t.includes("volume"))
    return "mdi:volume-high";
  if (n.includes("battery") || t.includes("battery"))
    return "mdi:battery";
  if (n.includes("bluetooth") || t.includes("bluetooth"))
    return "mdi:bluetooth";
  if (n.includes("telegram") || t.includes("telegram"))
    return "mdi:telegram";
  if (n.includes("discord") || t.includes("discord"))
    return "mdi:discord";
  if (n.includes("steam") || t.includes("steam"))
    return "mdi:steam";
  if (n.includes("dropbox") || t.includes("dropbox"))
    return "mdi:dropbox";
  if (n.includes("mail") || t.includes("mail"))
    return "mdi:email-outline";
  if (n.includes("kde") || t.includes("kde"))
    return "mdi:kde";
  return "mdi:circle-medium";
}
function iconPathCandidates(iconName) {
  const direct = iconName.startsWith("/") ? [iconName] : [];
  if (iconName.startsWith("/")) {
    return direct;
  }
  const exts = ["png", "svg"];
  const bases = [
    "/usr/share/pixmaps",
    "/usr/share/icons/hicolor/16x16/apps",
    "/usr/share/icons/hicolor/22x22/apps",
    "/usr/share/icons/hicolor/24x24/apps",
    "/usr/share/icons/hicolor/32x32/apps",
    "/usr/share/icons/hicolor/48x48/apps",
    "/usr/share/icons/hicolor/scalable/apps",
    "/usr/share/icons/breeze/apps/22",
    "/usr/share/icons/breeze/apps/24",
    "/usr/share/icons/breeze/apps/32"
  ];
  const results = [];
  for (const base of bases) {
    for (const ext of exts) {
      results.push(`${base}/${iconName}.${ext}`);
    }
  }
  return results;
}
function toDataUrlForIconFile(iconPath) {
  try {
    if (!fs.existsSync(iconPath)) {
      return;
    }
    const lower = iconPath.toLowerCase();
    const mime = lower.endsWith(".svg") ? "image/svg+xml" : lower.endsWith(".png") ? "image/png" : "";
    if (!mime) {
      return;
    }
    const base64 = fs.readFileSync(iconPath).toString("base64");
    return `data:${mime};base64,${base64}`;
  } catch {
    return;
  }
}
function resolveTrayIconDataUrl(iconName) {
  for (const candidate of iconPathCandidates(iconName)) {
    const dataUrl = toDataUrlForIconFile(candidate);
    if (dataUrl) {
      return dataUrl;
    }
  }
  return;
}
function getStatusNotifierTrayItems() {
  const watcherCall = runBusctlUser([
    "call",
    "org.kde.StatusNotifierWatcher",
    "/StatusNotifierWatcher",
    "org.kde.StatusNotifierWatcher",
    "RegisteredStatusNotifierItems"
  ]);
  if (!watcherCall) {
    return [];
  }
  const refs = parseQuotedStrings(watcherCall);
  const results = [];
  for (const itemRef of refs) {
    const parsed = parseServiceAndPath(itemRef);
    if (!parsed) {
      continue;
    }
    const statusRaw = getBusProperty(parsed.service, parsed.objectPath, "org.kde.StatusNotifierItem", "Status");
    const title = getBusProperty(parsed.service, parsed.objectPath, "org.kde.StatusNotifierItem", "Title") || getBusProperty(parsed.service, parsed.objectPath, "org.kde.StatusNotifierItem", "Id") || parsed.service;
    const iconName = getBusProperty(parsed.service, parsed.objectPath, "org.kde.StatusNotifierItem", "AttentionIconName") || getBusProperty(parsed.service, parsed.objectPath, "org.kde.StatusNotifierItem", "IconName");
    const menuPath = getBusProperty(parsed.service, parsed.objectPath, "org.kde.StatusNotifierItem", "Menu");
    const iconDataUrl = iconName ? resolveTrayIconDataUrl(iconName) : undefined;
    let status = "active";
    const s = statusRaw.toLowerCase();
    if (s.includes("passive")) {
      status = "passive";
    } else if (s.includes("attention") || s.includes("needsattention")) {
      status = "attention";
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
      hasMenu: Boolean(menuPath && menuPath !== "/NO_DBUSMENU")
    });
  }
  return results;
}
function activateStatusNotifierItem(service, objectPath) {
  const out = runBusctlUser([
    "call",
    service,
    objectPath,
    "org.kde.StatusNotifierItem",
    "Activate",
    "ii",
    "0",
    "0"
  ]);
  return out.length > 0;
}
function secondaryActivateStatusNotifierItem(service, objectPath) {
  const out = runBusctlUser([
    "call",
    service,
    objectPath,
    "org.kde.StatusNotifierItem",
    "SecondaryActivate",
    "ii",
    "0",
    "0"
  ]);
  return out.length > 0;
}
function openStatusNotifierContextMenu(service, objectPath, x, y) {
  const out = runBusctlUser([
    "call",
    service,
    objectPath,
    "org.kde.StatusNotifierItem",
    "ContextMenu",
    "ii",
    String(Math.trunc(x)),
    String(Math.trunc(y))
  ]);
  return out.length > 0;
}
function getDBusMenuLayout(service, menuPath) {
  try {
    const out = runBusctlUser([
      "call",
      service,
      menuPath.startsWith("/") ? menuPath : "/com/canonical/dbusmenu",
      "com.canonical.dbusmenu",
      "GetLayout",
      "u",
      "0"
    ]);
    if (!out) {
      return [];
    }
    const lines = out.split(`
`).filter((l) => l.trim().length > 0);
    const items = [];
    let depth = 0;
    for (const line of lines) {
      const trimmed = line.trim();
      const newDepth = (line.match(/^\s*/)?.[0]?.length ?? 0) / 2;
      if (trimmed.startsWith("(") && trimmed.includes('"')) {
        const quoted = parseQuotedStrings(trimmed);
        if (quoted.length > 0) {
          const label = quoted[0];
          const item = {
            id: label,
            label,
            enabled: !line.toLowerCase().includes("disabled")
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
function triggerDBusMenuEvent(service, menuPath, menuId) {
  try {
    const out = runBusctlUser([
      "call",
      service,
      menuPath.startsWith("/") ? menuPath : "/com/canonical/dbusmenu",
      "com.canonical.dbusmenu",
      "Event",
      "sis",
      menuId,
      "clicked",
      ""
    ]);
    return out.length > 0;
  } catch {
    return false;
  }
}
function scheduleIpcReconnect(delayMs = 1000) {
  if (CLIENT_MODE || process.env.WO_STANDALONE === "1") {
    return;
  }
  if (ipcReconnectTimer) {
    return;
  }
  ipcReconnectTimer = setTimeout(() => {
    ipcReconnectTimer = null;
    connectToCompositor().then(() => {
      debugLog("[Wo] IPC reconnect succeeded");
    }).catch((error) => {
      debugLog("[Wo] IPC reconnect failed:", String(error));
      scheduleIpcReconnect();
    });
  }, delayMs);
}
function sendActionToCompositor(action, payload) {
  if (!ipcSocket || !ipcConnected) {
    return false;
  }
  try {
    const actionBuf = stringToBuffer(action);
    const payloadStr = payload ? JSON.stringify(payload) : "";
    const payloadBuf = stringToBuffer(payloadStr);
    const messageBuf = Buffer.alloc(12 + actionBuf.length + payloadBuf.length);
    let offset = 0;
    messageBuf.writeUInt32LE(MAGIC.ACTION, offset);
    offset += 4;
    messageBuf.writeUInt32LE(actionBuf.length, offset);
    offset += 4;
    actionBuf.copy(messageBuf, offset);
    offset += actionBuf.length;
    messageBuf.writeUInt32LE(payloadBuf.length, offset);
    offset += 4;
    payloadBuf.copy(messageBuf, offset);
    ipcSocket.write(messageBuf);
    return true;
  } catch {
    return false;
  }
}
function sendWindowPositionUpdate(x, y, width2, height2) {
  if (!ipcSocket || !ipcConnected) {
    return false;
  }
  try {
    const nameBuf = stringToBuffer(name);
    const messageBuf = Buffer.alloc(8 + nameBuf.length + 16);
    let offset = 0;
    messageBuf.writeUInt32LE(MAGIC.WINDOW_POS, offset);
    offset += 4;
    messageBuf.writeUInt32LE(nameBuf.length, offset);
    offset += 4;
    nameBuf.copy(messageBuf, offset);
    offset += nameBuf.length;
    messageBuf.writeInt32LE(x, offset);
    offset += 4;
    messageBuf.writeInt32LE(y, offset);
    offset += 4;
    messageBuf.writeUInt32LE(width2, offset);
    offset += 4;
    messageBuf.writeUInt32LE(height2, offset);
    ipcSocket.write(messageBuf);
    return true;
  } catch {
    return false;
  }
}
function linuxButtonToElectron(btn) {
  if (btn === 273)
    return "right";
  if (btn === 274)
    return "middle";
  return "left";
}
var modifierState = { shift: false, control: false, alt: false, meta: false };
var MODIFIER_KEYCODES = {
  29: "control",
  42: "shift",
  54: "shift",
  56: "alt",
  97: "control",
  100: "alt",
  125: "meta",
  126: "meta"
};
function getModifierArray() {
  const mods = [];
  if (modifierState.shift)
    mods.push("shift");
  if (modifierState.control)
    mods.push("control");
  if (modifierState.alt)
    mods.push("alt");
  if (modifierState.meta)
    mods.push("meta");
  return mods;
}
var SHIFTED_CHARS = {
  "1": "!",
  "2": "@",
  "3": "#",
  "4": "$",
  "5": "%",
  "6": "^",
  "7": "&",
  "8": "*",
  "9": "(",
  "0": ")",
  "-": "_",
  "=": "+",
  "[": "{",
  "]": "}",
  "\\": "|",
  ";": ":",
  "'": '"',
  "`": "~",
  ",": "<",
  ".": ">",
  "/": "?"
};
var LINUX_KEYCODE_MAP = {
  1: "Escape",
  59: "F1",
  60: "F2",
  61: "F3",
  62: "F4",
  63: "F5",
  64: "F6",
  65: "F7",
  66: "F8",
  67: "F9",
  68: "F10",
  87: "F11",
  88: "F12",
  41: "`",
  2: "1",
  3: "2",
  4: "3",
  5: "4",
  6: "5",
  7: "6",
  8: "7",
  9: "8",
  10: "9",
  11: "0",
  12: "-",
  13: "=",
  14: "Backspace",
  15: "Tab",
  16: "q",
  17: "w",
  18: "e",
  19: "r",
  20: "t",
  21: "y",
  22: "u",
  23: "i",
  24: "o",
  25: "p",
  26: "[",
  27: "]",
  43: "\\",
  58: "CapsLock",
  30: "a",
  31: "s",
  32: "d",
  33: "f",
  34: "g",
  35: "h",
  36: "j",
  37: "k",
  38: "l",
  39: ";",
  40: "'",
  28: "Return",
  42: "Shift",
  44: "z",
  45: "x",
  46: "c",
  47: "v",
  48: "b",
  49: "n",
  50: "m",
  51: ",",
  52: ".",
  53: "/",
  54: "Shift",
  29: "Control",
  125: "Meta",
  56: "Alt",
  57: "Space",
  100: "Alt",
  126: "Meta",
  97: "Control",
  110: "Insert",
  102: "Home",
  104: "PageUp",
  111: "Delete",
  107: "End",
  109: "PageDown",
  103: "Up",
  105: "Left",
  108: "Down",
  106: "Right",
  69: "NumLock",
  98: "numdiv",
  55: "nummult",
  74: "numsub",
  71: "num7",
  72: "num8",
  73: "num9",
  78: "numadd",
  75: "num4",
  76: "num5",
  77: "num6",
  79: "num1",
  80: "num2",
  81: "num3",
  96: "Enter",
  82: "num0",
  83: "numdec",
  99: "PrintScreen",
  70: "ScrollLock",
  119: "Pause",
  113: "VolumeMute",
  114: "VolumeDown",
  115: "VolumeUp",
  163: "MediaNextTrack",
  165: "MediaPreviousTrack",
  164: "MediaPlayPause",
  166: "MediaStop"
};
function connectToCompositor() {
  return new Promise((resolve, reject) => {
    if (ipcReconnectTimer) {
      clearTimeout(ipcReconnectTimer);
      ipcReconnectTimer = null;
    }
    if (ipcSocket && !ipcSocket.destroyed) {
      ipcSocket.destroy();
    }
    debugLog("[Wo] Connecting to compositor IPC:", IPC_SOCKET);
    ipcSocket = createConnection(IPC_SOCKET, () => {
      const windowName = name;
      const nameBuf = stringToBuffer(windowName);
      const messageBuf = Buffer.alloc(8 + nameBuf.length + 8);
      let offset = 0;
      messageBuf.writeUInt32LE(MAGIC.HELLO, offset);
      offset += 4;
      messageBuf.writeUInt32LE(nameBuf.length, offset);
      offset += 4;
      nameBuf.copy(messageBuf, offset);
      offset += nameBuf.length;
      messageBuf.writeUInt32LE(width, offset);
      offset += 4;
      messageBuf.writeUInt32LE(height, offset);
      ipcSocket.write(messageBuf);
      ipcConnected = true;
      debugLog("[Wo] IPC connected, sent HELLO message");
      try {
        const nativePath = resolvePath(__dirname2, "../native/build/Release/wo_dmabuf.node");
        nativeDmabuf = loadNativeDmabufModule(require2, nativePath);
        nativeDmabuf.init(process.env.WO_DRM_RENDER_NODE || "/dev/dri/renderD128");
        dmabufSender = new WoDmabufSender(ipcSocket, name, nativeDmabuf);
        debugLog("[Wo] DMABUF sender initialized");
      } catch (error) {
        debugLog("[Wo] DMABUF sender unavailable:", error);
        dmabufSender = null;
      }
      resolve();
    });
    let ipcDataCalls = 0;
    let ipcDataLastLog = Date.now();
    let pendingMouseMove = null;
    let inputFlushTimer = null;
    const flushInputEvents = () => {
      inputFlushTimer = null;
      if (pendingMouseMove && mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.sendInputEvent({
          type: "mouseMove",
          x: pendingMouseMove.x,
          y: pendingMouseMove.y
        });
        pendingMouseMove = null;
      }
    };
    ipcSocket.on("data", (chunk) => {
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
          if (rxAvailable() < 12)
            break;
          const seq = compositorRxBuffer.readBigUInt64LE(rxReadOffset + 4).toString();
          inFlightFrameSeqs.delete(seq);
          rxConsume(12);
          continue;
        }
        if (magic === MAGIC.MOUSE_MOVE) {
          if (rxAvailable() < 20)
            break;
          const x = compositorRxBuffer.readDoubleLE(rxReadOffset + 4);
          const y = compositorRxBuffer.readDoubleLE(rxReadOffset + 12);
          rxConsume(20);
          pointerX = Math.round(x);
          pointerY = Math.round(y);
          if (!inputFlushTimer) {
            if (mainWindow && !mainWindow.isDestroyed()) {
              mainWindow.webContents.sendInputEvent({ type: "mouseMove", x: pointerX, y: pointerY });
            }
          } else {
            pendingMouseMove = { x: pointerX, y: pointerY };
            inputFlushTimer = setTimeout(flushInputEvents, 0);
          }
          continue;
        }
        if (magic === MAGIC.MOUSE_BUTTON) {
          if (rxAvailable() < 16)
            break;
          const button = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const pressed = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          rxConsume(16);
          if (pendingMouseMove && mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.sendInputEvent({
              type: "mouseMove",
              x: pendingMouseMove.x,
              y: pendingMouseMove.y
            });
            pendingMouseMove = null;
          }
          if (inputFlushTimer) {
            clearTimeout(inputFlushTimer);
            inputFlushTimer = null;
          }
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.sendInputEvent({
              type: pressed ? "mouseDown" : "mouseUp",
              x: pointerX,
              y: pointerY,
              button: linuxButtonToElectron(button),
              clickCount: 1,
              modifiers: getModifierArray()
            });
          }
          continue;
        }
        if (magic === MAGIC.KEYBOARD) {
          if (rxAvailable() < 16)
            break;
          const key = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const pressed = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          rxConsume(16);
          const modKey = MODIFIER_KEYCODES[key];
          if (modKey) {
            modifierState[modKey] = pressed;
          }
          const keyCode = LINUX_KEYCODE_MAP[key];
          if (keyCode && mainWindow && !mainWindow.isDestroyed()) {
            const modifiers = getModifierArray();
            mainWindow.webContents.sendInputEvent({
              type: pressed ? "keyDown" : "keyUp",
              keyCode,
              modifiers
            });
            if (pressed && keyCode.length === 1) {
              let charKey = keyCode;
              if (modifierState.shift) {
                if (charKey >= "a" && charKey <= "z") {
                  charKey = charKey.toUpperCase();
                } else if (SHIFTED_CHARS[charKey]) {
                  charKey = SHIFTED_CHARS[charKey];
                }
              }
              mainWindow.webContents.sendInputEvent({ type: "char", keyCode: charKey, modifiers });
            }
          }
          continue;
        }
        if (magic === MAGIC.SCROLL) {
          if (rxAvailable() < 16)
            break;
          const vertical = compositorRxBuffer.readInt32LE(rxReadOffset + 4);
          const horizontal = compositorRxBuffer.readInt32LE(rxReadOffset + 8);
          rxConsume(16);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.sendInputEvent({
              type: "mouseWheel",
              x: pointerX,
              y: pointerY,
              deltaX: horizontal,
              deltaY: vertical,
              canScroll: true
            });
          }
          continue;
        }
        if (magic === MAGIC.FOCUS_CHANGE) {
          if (rxAvailable() < 12)
            break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 12 + nameLen)
            break;
          const focused = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          const windowName = compositorRxBuffer.slice(rxReadOffset + 12, rxReadOffset + 12 + nameLen).toString("utf8");
          rxConsume(12 + nameLen);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send("wo:focus-change", { window: windowName, focused });
          }
          continue;
        }
        if (magic === MAGIC.WINDOW_META) {
          if (rxAvailable() < 8)
            break;
          const payloadLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 8 + payloadLen)
            break;
          const metadata = compositorRxBuffer.slice(rxReadOffset + 8, rxReadOffset + 8 + payloadLen).toString("utf8");
          rxConsume(8 + payloadLen);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send("wo:window-metadata", { type: "windowMetadata", metadata });
            try {
              const parsed = JSON.parse(metadata);
              if (parsed && Array.isArray(parsed.windows)) {
                const currentNames = new Set(parsed.windows.map((w) => w.name).filter(Boolean));
                for (const cachedName of surfaceSabCache.keys()) {
                  if (!currentNames.has(cachedName)) {
                    surfaceSabCache.delete(cachedName);
                  }
                }
                for (const cachedName of shmFdCache.keys()) {
                  if (!currentNames.has(cachedName)) {
                    const entry = shmFdCache.get(cachedName);
                    if (entry) {
                      try {
                        fs.closeSync(entry.extFd);
                      } catch {}
                    }
                    shmFdCache.delete(cachedName);
                  }
                }
                compositorWindows = parsed.windows;
                mainWindow.webContents.send("wo:windows", compositorWindows);
              }
            } catch {}
          }
          continue;
        }
        if (magic === MAGIC.SHM_BUFFER) {
          if (rxAvailable() < 8)
            break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const baseHeader = 8 + nameLen + 20 + 4;
          if (rxAvailable() < baseHeader)
            break;
          let off = rxReadOffset + 8;
          const windowName = compositorRxBuffer.slice(off, off + nameLen).toString("utf8");
          off += nameLen;
          const sbWidth = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const sbHeight = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const sbStride = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const pid = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const fd = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const numRects = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const fullHeader = baseHeader + numRects * 16;
          if (rxAvailable() < fullHeader)
            break;
          const damageRects = [];
          for (let i = 0;i < numRects; i++) {
            damageRects.push({
              x: compositorRxBuffer.readUInt32LE(off),
              y: compositorRxBuffer.readUInt32LE(off + 4),
              width: compositorRxBuffer.readUInt32LE(off + 8),
              height: compositorRxBuffer.readUInt32LE(off + 12)
            });
            off += 16;
          }
          rxConsume(fullHeader);
          try {
            const extFd = getOrOpenShmFd(windowName, pid, fd);
            const fullSize = sbStride * sbHeight;
            const sabEntry = getOrCreateSab(windowName, sbWidth, sbHeight, sbStride);
            const hasUsableRects = damageRects.length > 0 && damageRects.length < 64;
            if (hasUsableRects && nativeDmabuf?.copyMmapDamageToSab) {
              let minY = sbHeight, maxY = 0;
              for (const r of damageRects) {
                const ry = Math.max(0, r.y);
                const ryEnd = Math.min(sbHeight, r.y + r.height);
                if (ry < minY)
                  minY = ry;
                if (ryEnd > maxY)
                  maxY = ryEnd;
              }
              if (minY < maxY) {
                nativeDmabuf.copyMmapDamageToSab(extFd, sabEntry.sab, sbStride, [
                  { y: minY, h: maxY - minY }
                ]);
              }
            } else if (nativeDmabuf?.copyMmapToSab) {
              nativeDmabuf.copyMmapToSab(extFd, sabEntry.sab, fullSize);
            } else {
              const tmpBuf = Buffer.alloc(fullSize);
              let totalRead = 0;
              while (totalRead < fullSize) {
                const bytesRead = fs.readSync(extFd, tmpBuf, totalRead, fullSize - totalRead, totalRead);
                if (bytesRead <= 0)
                  break;
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
              damageRects: hasUsableRects ? damageRects : undefined
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
              } catch {}
              shmFdCache.delete(windowName);
            }
            debugLog(`[Wo] SHM_BUFFER fs.readSync failed for ${windowName}:`, err);
          }
          continue;
        }
        if (magic === MAGIC.DMABUF_FRAME) {
          if (rxAvailable() < 8)
            break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          const numPlanesOffset = 8 + nameLen + 12;
          if (rxAvailable() < numPlanesOffset + 4)
            break;
          let off = rxReadOffset + 8;
          const dmabufName = compositorRxBuffer.slice(off, off + nameLen).toString("utf8");
          off += nameLen;
          const dmabufW = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const dmabufH = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const dmabufFormat = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const numPlanes = compositorRxBuffer.readUInt32LE(off);
          off += 4;
          const totalSize = off - rxReadOffset + numPlanes * 24;
          if (rxAvailable() < totalSize)
            break;
          try {
            if (nativeDmabuf && ipcSocket) {
              const socketFd = ipcSocket?._handle?.fd;
              if (typeof socketFd === "number" && socketFd >= 0) {
                const dmabufFd = nativeDmabuf.recvFd(socketFd);
                if (dmabufFd >= 0) {
                  try {
                    const textureInfo = nativeDmabuf.importDmabufTexture(dmabufName, dmabufFd, dmabufW, dmabufH, dmabufFormat);
                    if (mainWindow && !mainWindow.isDestroyed()) {
                      mainWindow.webContents.send("wo:dmabuf-frame", {
                        name: dmabufName,
                        texture: textureInfo.texture,
                        width: textureInfo.width,
                        height: textureInfo.height
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
          if (rxAvailable() < 12)
            break;
          const nameLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 12 + nameLen)
            break;
          const lock = compositorRxBuffer.readUInt32LE(rxReadOffset + 8) !== 0;
          const windowName = compositorRxBuffer.slice(rxReadOffset + 12, rxReadOffset + 12 + nameLen).toString("utf8");
          rxConsume(12 + nameLen);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send("wo:pointer-lock-request", { window: windowName, lock });
          }
          continue;
        }
        if (magic === MAGIC.ENV_UPDATE) {
          if (rxAvailable() < 8)
            break;
          const jsonLen = compositorRxBuffer.readUInt32LE(rxReadOffset + 4);
          if (rxAvailable() < 8 + jsonLen)
            break;
          const jsonStr = compositorRxBuffer.slice(rxReadOffset + 8, rxReadOffset + 8 + jsonLen).toString("utf8");
          rxConsume(8 + jsonLen);
          try {
            const vars = JSON.parse(jsonStr);
            for (const [key, value] of Object.entries(vars)) {
              process.env[key] = value;
              debugLog(`[Wo] env update: ${key}=${value}`);
            }
            if (mainWindow && !mainWindow.isDestroyed()) {
              mainWindow.webContents.send("wo:env-update", vars);
            }
          } catch (err) {
            debugLog("[Wo] ENV_UPDATE parse error:", err);
          }
          continue;
        }
        if (magic === MAGIC.SCREENCOPY_EVENT) {
          if (rxAvailable() < 12)
            break;
          const active = compositorRxBuffer.readUInt32LE(rxReadOffset + 4) !== 0;
          const clientCount = compositorRxBuffer.readUInt32LE(rxReadOffset + 8);
          rxConsume(12);
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send("wo:screencopy-event", { active, clientCount });
          }
          continue;
        }
        debugLog("[Wo] UNKNOWN magic 0x" + magic.toString(16).padStart(8, "0") + " bufLen=" + rxAvailable() + " — stream may be corrupted");
        rxConsume(1);
      }
      const dataElapsed = Date.now() - dataT0;
      if (dataElapsed > 50) {
        console.warn(`[IPC SLOW] data handler took ${dataElapsed}ms chunkLen=${chunk.length}`);
      }
    });
    ipcSocket.on("error", (error) => {
      debugLog("[Wo] IPC connection error:", error);
      const hadConnected = ipcConnected;
      ipcConnected = false;
      dmabufSender = null;
      if (hadConnected) {
        scheduleIpcReconnect();
        return;
      }
      reject(error);
    });
    ipcSocket.on("close", () => {
      debugLog("[Wo] IPC connection closed");
      ipcConnected = false;
      ipcSocket = null;
      dmabufSender = null;
      inFlightFrameSeqs.clear();
      compositorRxBuffer = Buffer.alloc(64 * 1024);
      rxWriteOffset = 0;
      rxReadOffset = 0;
      surfaceSabCache.clear();
      surfaceUpdatePending.clear();
      closeShmFdCache();
      scheduleIpcReconnect();
    });
    setTimeout(() => {
      if (!ipcConnected) {
        debugLog("[Wo] IPC connection timeout");
        reject(new Error("Failed to connect to compositor IPC socket"));
      }
    }, 5000);
  });
}
function setupIpcHandlers() {
  ipcMain.handle("action", (_event, action, payload) => {
    return sendActionToCompositor(action, payload);
  });
  ipcMain.on("action-sync", (_event, action, payload) => {
    sendActionToCompositor(action, payload);
  });
  ipcMain.handle("wo:get-windows", async () => compositorWindows.length > 0 ? compositorWindows : clientWindows);
  ipcMain.on("wo:keybind-action", (_event, action) => {
    if (action === "shuffle") {
      clientWindows = [...clientWindows].sort(() => Math.random() - 0.5);
    } else if (action === "reverse") {
      clientWindows = [...clientWindows].reverse();
    }
    if (mainWindow) {
      mainWindow.webContents.send("wo:windows", clientWindows);
    }
  });
  ipcMain.handle("wo:submit-damage-frame", (_event, payload) => {
    if (!dmabufSender) {
      return { ok: false, reason: "dmabuf-sender-not-ready" };
    }
    if (inFlightFrameSeqs.size >= MAX_IN_FLIGHT_FRAMES) {
      return { ok: true, skipped: true, reason: "backpressure" };
    }
    if (!damageBuffer) {
      damageBuffer = new DamageBuffer(payload.width, payload.height);
    }
    appUsedDamageHelper = true;
    applyDamagePayload(damageBuffer, payload);
    const seq = dmabufSender.send(damageBuffer);
    inFlightFrameSeqs.add(seq.toString());
    return { ok: true, seq: seq.toString() };
  });
  ipcMain.on("wo:forward-keyboard", (_event, windowName, key, pressed, time) => {
    if (ipcSocket && ipcConnected) {
      const nameBuf = Buffer.from(windowName, "utf8");
      const msg = Buffer.alloc(4 + 4 + nameBuf.length + 12);
      let off = 0;
      msg.writeUInt32LE(MAGIC.FORWARD_KEYBOARD, off);
      off += 4;
      msg.writeUInt32LE(nameBuf.length, off);
      off += 4;
      nameBuf.copy(msg, off);
      off += nameBuf.length;
      msg.writeUInt32LE(key, off);
      off += 4;
      msg.writeUInt32LE(pressed ? 1 : 0, off);
      off += 4;
      msg.writeUInt32LE(time || 0, off);
      ipcSocket.write(msg);
    }
  });
  ipcMain.on("wo:forward-relative-pointer", (_event, windowName, dx, dy) => {
    if (ipcSocket && ipcConnected) {
      const nameBuf = Buffer.from(windowName, "utf8");
      const msg = Buffer.alloc(4 + 4 + nameBuf.length + 16);
      let off = 0;
      msg.writeUInt32LE(MAGIC.FORWARD_RELATIVE_POINTER, off);
      off += 4;
      msg.writeUInt32LE(nameBuf.length, off);
      off += 4;
      nameBuf.copy(msg, off);
      off += nameBuf.length;
      msg.writeDoubleLE(dx, off);
      off += 8;
      msg.writeDoubleLE(dy, off);
      ipcSocket.write(msg);
    }
  });
  const iconCache = new Map;
  function resolveDesktopIconFast(iconName, fs2, path) {
    if (iconCache.has(iconName))
      return iconCache.get(iconName) ?? null;
    let result = null;
    if (iconName.startsWith("/")) {
      try {
        if (fs2.existsSync(iconName)) {
          const ext = path.extname(iconName).toLowerCase();
          const mime = ext === ".svg" ? "image/svg+xml" : "image/png";
          result = { type: "base64", data: fs2.readFileSync(iconName).toString("base64"), mimeType: mime };
        }
      } catch {}
      iconCache.set(iconName, result);
      return result;
    }
    const quickPaths = [
      [`/usr/share/pixmaps/${iconName}.png`, "image/png"],
      [`/usr/share/pixmaps/${iconName}.svg`, "image/svg+xml"],
      [`/usr/share/icons/hicolor/48x48/apps/${iconName}.png`, "image/png"],
      [`/usr/share/icons/hicolor/scalable/apps/${iconName}.svg`, "image/svg+xml"]
    ];
    for (const [iconPath, mime] of quickPaths) {
      try {
        if (fs2.existsSync(iconPath)) {
          result = { type: "base64", data: fs2.readFileSync(iconPath).toString("base64"), mimeType: mime };
          break;
        }
      } catch {}
    }
    iconCache.set(iconName, result);
    return result;
  }
  ipcMain.handle("wo:syscall", async (_event, type, params) => {
    try {
      switch (type) {
        case "list_applications": {
          const path = await import("path");
          const fs2 = await import("fs");
          const apps = [];
          const dirs = ["/usr/share/applications", `${process.env.HOME}/.local/share/applications`];
          const seen = new Set;
          for (const dir of dirs) {
            try {
              if (!fs2.default.existsSync(dir))
                continue;
              const files = fs2.default.readdirSync(dir).filter((f) => f.endsWith(".desktop"));
              for (const file of files) {
                const filePath = path.default.join(dir, file);
                try {
                  const content = fs2.default.readFileSync(filePath, "utf-8");
                  if (content.includes("NoDisplay=true"))
                    continue;
                  const nameMatch = content.match(/^Name=(.+)$/m);
                  const execMatch = content.match(/^Exec=(.+)$/m);
                  const iconMatch = content.match(/^Icon=(.+)$/m);
                  if (nameMatch && execMatch) {
                    const name2 = nameMatch[1].trim();
                    if (!seen.has(name2)) {
                      seen.add(name2);
                      const exec = execMatch[1].trim().split(" ")[0].split("/").pop() || execMatch[1].trim();
                      const app2 = {
                        name: name2,
                        command: exec
                      };
                      if (iconMatch) {
                        const iconName = iconMatch[1].trim();
                        const resolved = resolveDesktopIconFast(iconName, fs2.default, path.default);
                        if (resolved) {
                          app2.icon = resolved;
                        } else {
                          app2.icon = { type: "iconify", data: iconName };
                        }
                      }
                      apps.push(app2);
                    }
                  }
                } catch {}
              }
            } catch {}
          }
          return apps.length > 0 ? apps : [
            { name: "Terminal", icon: { type: "iconify", data: "mdi:console" }, command: "xterm" },
            { name: "Firefox", icon: { type: "iconify", data: "mdi:firefox" }, command: "firefox" },
            { name: "Files", icon: { type: "iconify", data: "mdi:folder" }, command: "nautilus" }
          ];
        }
        case "launch":
        case "exec": {
          const { spawn } = await import("child_process");
          const command = params.command || "";
          if (!command) {
            return { ok: false, error: "No command specified" };
          }
          try {
            const child = spawn(command, {
              detached: true,
              stdio: "ignore",
              shell: true
            });
            child.unref();
            return { ok: true, pid: child.pid };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }
        case "listdir": {
          const fs2 = await import("fs");
          const path = await import("path");
          const dirPath = params.path || process.env.HOME || "/home";
          try {
            const entries = fs2.default.readdirSync(dirPath, { withFileTypes: true });
            return {
              ok: true,
              path: dirPath,
              entries: entries.map((e) => ({
                name: e.name,
                type: e.isDirectory() ? "dir" : "file"
              }))
            };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }
        case "read": {
          const fs2 = await import("fs");
          const filePath = params.path;
          if (!filePath) {
            return { ok: false, error: "No file path specified" };
          }
          try {
            const content = fs2.default.readFileSync(filePath, "utf-8");
            return { ok: true, content, size: content.length };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }
        case "write": {
          const fs2 = await import("fs");
          const filePath = params.path;
          const content = params.content;
          if (!filePath || content === undefined) {
            return { ok: false, error: "Missing file path or content" };
          }
          try {
            fs2.default.writeFileSync(filePath, content);
            return { ok: true, bytesWritten: content.length };
          } catch (error) {
            return { ok: false, error: String(error) };
          }
        }
        case "shutdown": {
          const { spawn } = await import("child_process");
          spawn("systemctl", ["poweroff"], { detached: true, stdio: "ignore" }).unref();
          return { ok: true };
        }
        case "restart": {
          const { spawn } = await import("child_process");
          spawn("systemctl", ["reboot"], { detached: true, stdio: "ignore" }).unref();
          return { ok: true };
        }
        case "logout": {
          const { spawn } = await import("child_process");
          const sessionId = process.env.XDG_SESSION_ID || "";
          if (sessionId) {
            spawn("loginctl", ["terminate-session", sessionId], { detached: true, stdio: "ignore" }).unref();
          } else {
            sendActionToCompositor("quit", { code: 0 });
          }
          return { ok: true };
        }
        case "lock": {
          const { spawn } = await import("child_process");
          spawn("loginctl", ["lock-session"], { detached: true, stdio: "ignore" }).unref();
          return { ok: true };
        }
        case "sleep": {
          const { spawn } = await import("child_process");
          spawn("systemctl", ["suspend"], { detached: true, stdio: "ignore" }).unref();
          return { ok: true };
        }
        case "notify": {
          if (mainWindow && !mainWindow.isDestroyed()) {
            mainWindow.webContents.send("wo:notification", {
              id: params.id || `notify-${Date.now()}`,
              title: params.title || "Notification",
              body: params.body || "",
              icon: params.icon,
              timeout: params.timeout ?? 5000,
              timestamp: Date.now()
            });
          }
          return { ok: true };
        }
        case "tray_list": {
          return getStatusNotifierTrayItems();
        }
        case "tray_activate": {
          const service = String(params.service || "");
          const objectPath = String(params.objectPath || "");
          if (!service || !objectPath) {
            return { ok: false, error: "Missing service or objectPath" };
          }
          const ok = activateStatusNotifierItem(service, objectPath);
          return ok ? { ok: true } : { ok: false, error: "Activation failed" };
        }
        case "tray_secondary_activate": {
          const service = String(params.service || "");
          const objectPath = String(params.objectPath || "");
          if (!service || !objectPath) {
            return { ok: false, error: "Missing service or objectPath" };
          }
          const ok = secondaryActivateStatusNotifierItem(service, objectPath);
          return ok ? { ok: true } : { ok: false, error: "Secondary activation failed" };
        }
        case "tray_context_menu": {
          const service = String(params.service || "");
          const objectPath = String(params.objectPath || "");
          const x = Number(params.x ?? 0);
          const y = Number(params.y ?? 0);
          if (!service || !objectPath) {
            return { ok: false, error: "Missing service or objectPath" };
          }
          const ok = openStatusNotifierContextMenu(service, objectPath, x, y);
          return ok ? { ok: true } : { ok: false, error: "Context menu failed" };
        }
        case "tray_menu_items": {
          const service = String(params.service || "");
          const menuPath = String(params.menuPath || "");
          if (!service || !menuPath) {
            return [];
          }
          return getDBusMenuLayout(service, menuPath);
        }
        case "tray_menu_event": {
          const service = String(params.service || "");
          const menuPath = String(params.menuPath || "");
          const menuId = String(params.menuId || "");
          if (!service || !menuPath || !menuId) {
            return { ok: false, error: "Missing parameters" };
          }
          const ok = triggerDBusMenuEvent(service, menuPath, menuId);
          return ok ? { ok: true } : { ok: false, error: "Menu event failed" };
        }
        case "portal_respond": {
          const requestId = String(params.requestId || "");
          if (!requestId) {
            return { ok: false, error: "Missing portal requestId" };
          }
          return respondToPortalRequest(requestId, {
            allowed: Boolean(params.allowed),
            type: typeof params.type === "string" ? params.type : undefined,
            windowName: params.windowName
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
  debugLog("[Wo] createWindow starting");
  const preloadPath = joinPath(__dirname2, "preload.js");
  debugLog("[Wo] preloadPath=" + preloadPath);
  try {
    mainWindow = new BrowserWindow({
      width: CLIENT_MODE ? 1400 : width,
      height: CLIENT_MODE ? 900 : height,
      transparent: true,
      backgroundColor: "#00000000",
      frame: CLIENT_MODE,
      webPreferences: {
        sandbox: false,
        contextIsolation: true,
        preload: preloadPath,
        nodeIntegration: false,
        offscreen: !CLIENT_MODE,
        webgl: true,
        backgroundThrottling: false
      },
      show: CLIENT_MODE
    });
    mainWindow.webContents.session.webRequest.onHeadersReceived((details, callback) => {
      callback({
        responseHeaders: {
          ...details.responseHeaders,
          "Cross-Origin-Opener-Policy": ["same-origin"],
          "Cross-Origin-Embedder-Policy": ["require-corp"]
        }
      });
    });
    debugLog("[Wo] BrowserWindow created, CLIENT_MODE=" + CLIENT_MODE + " offscreen=" + !CLIENT_MODE);
    setupIpcHandlers();
    debugLog("[Wo] setupIpcHandlers completed");
    mainWindow.webContents.send("wo:windows", clientWindows);
    if (CLIENT_MODE) {
      return;
    }
    debugLog("[Wo] Loading content URL: " + (url || "minimal fallback"));
    try {
      const pageUrl = url || "data:text/html,<!DOCTYPE html><html><head><style>body{background:white;margin:0;}</style></head><body></body></html>";
      const loadPromise = mainWindow.loadURL(pageUrl);
      await Promise.race([
        loadPromise,
        new Promise((r) => setTimeout(() => {
          debugLog("[Wo] Load timeout, continuing");
          r(undefined);
        }, 3000))
      ]);
      debugLog("[Wo] Content loaded from: " + pageUrl);
    } catch (e) {
      debugLog("[Wo] Load failed:", String(e));
    }
    debugLog("[Wo] Page initialized, setting up rendering");
    mainWindow.webContents.focus();
    if (!CLIENT_MODE) {
      debugLog("[Wo] Setting up OSR rendering, FPS=" + (WINDOW_CONFIG.fps ?? 60));
      mainWindow.webContents.setFrameRate(WINDOW_CONFIG.fps ?? 60);
      mainWindow.webContents.on("did-finish-load", () => {
        mainWindow.webContents.setFrameRate(WINDOW_CONFIG.fps ?? 60);
      });
      const frameIntervalMs = Math.round(1000 / (WINDOW_CONFIG.fps ?? 60));
      setInterval(() => {
        if (mainWindow && !mainWindow.isDestroyed() && inFlightFrameSeqs.size < MAX_IN_FLIGHT_FRAMES) {
          mainWindow.webContents.invalidate();
        }
      }, frameIntervalMs);
      let patchBuf = null;
      let skippedCount = 0;
      mainWindow.webContents.on("paint", (_event, dirty, image) => {
        if (!dmabufSender || !ipcConnected) {
          skippedCount++;
          debugLog("[Wo] SKIPPED paint (dmabufSender=" + !!dmabufSender + " ipcConnected=" + ipcConnected + ")");
          return;
        }
        if (appUsedDamageHelper) {
          return;
        }
        if (inFlightFrameSeqs.size >= MAX_IN_FLIGHT_FRAMES) {
          skippedCount++;
          if (skippedCount === 1 || skippedCount % 120 === 0) {
            debugLog("[Wo] SKIPPED paint due to frame backpressure inFlight=" + inFlightFrameSeqs.size);
          }
          return;
        }
        const size = image.getSize();
        if (!damageBuffer) {
          damageBuffer = new DamageBuffer(size.width, size.height);
        }
        const pixels = image.toBitmap();
        const pixelData = new Uint8Array(pixels.buffer, pixels.byteOffset, pixels.byteLength);
        const isFullFrame = dirty.x === 0 && dirty.y === 0 && dirty.width === size.width && dirty.height === size.height;
        if (isFullFrame) {
          applyDamagePayload(damageBuffer, {
            width: size.width,
            height: size.height,
            fullFrame: pixelData
          });
        } else {
          const stride = size.width * 4;
          const patchStride = dirty.width * 4;
          const patchBytes = patchStride * dirty.height;
          if (!patchBuf || patchBuf.byteLength < patchBytes) {
            patchBuf = new Uint8Array(patchBytes);
          }
          const patchData = patchBuf.byteLength === patchBytes ? patchBuf : patchBuf.subarray(0, patchBytes);
          for (let row = 0;row < dirty.height; row++) {
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
              stride: patchStride
            }]
          });
        }
        try {
          const seq = dmabufSender.send(damageBuffer);
          inFlightFrameSeqs.add(seq.toString());
        } catch (e) {
          debugLog("[Wo] ERROR sending frame:", String(e));
        }
      });
    }
    if (process.env.WO_DEBUG === "1") {
      mainWindow.webContents.openDevTools();
    }
    debugLog("[Wo] createWindow completed successfully");
  } catch (error) {
    debugLog("[Wo] createWindow error:", error);
    throw error;
  }
  mainWindow.on("closed", () => {
    mainWindow = null;
    if (windowPositionUpdateTimer) {
      clearInterval(windowPositionUpdateTimer);
      windowPositionUpdateTimer = null;
    }
  });
  if (!CLIENT_MODE && ipcConnected) {
    const sendPosition = () => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        const bounds = mainWindow.getBounds();
        sendWindowPositionUpdate(bounds.x, bounds.y, bounds.width, bounds.height);
      }
    };
    sendPosition();
    windowPositionUpdateTimer = setInterval(sendPosition, 100);
    mainWindow.on("move", sendPosition);
    mainWindow.on("resize", sendPosition);
  }
}
app.on("ready", async () => {
  debugLog("[Wo] App ready, CLIENT_MODE=", CLIENT_MODE, "WO_STANDALONE=", process.env.WO_STANDALONE);
  const gpuInfo = app.getGPUFeatureStatus();
  debugLog("[Wo] GPU feature status:", JSON.stringify(gpuInfo));
  debugLog("[Wo] GPU info:", JSON.stringify(app.getGPUInfo("basic").catch(() => ({}))));
  try {
    startPortalUiBridge();
    if (!CLIENT_MODE && process.env.WO_STANDALONE !== "1") {
      debugLog("[Wo] Will connect to compositor");
      await connectToCompositor();
    }
    debugLog("[Wo] Creating window");
    await createWindow();
    debugLog("[Wo] Window created successfully");
  } catch (error) {
    debugLog("[Wo] Failed to start app:", error);
    app.quit();
  }
});
app.on("window-all-closed", () => {
  stopPortalUiBridge();
  if (process.platform !== "darwin") {
    app.quit();
  }
});
app.on("activate", () => {
  if (mainWindow === null) {
    createWindow().catch(console.error);
  }
});
app.on("before-quit", () => {
  stopPortalUiBridge();
  closeShmFdCache();
});
export {
  sendActionToCompositor,
  connectToCompositor
};
