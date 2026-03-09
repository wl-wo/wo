/**
 * Wo Compositor - Shared Types and Protocol
 * 
 * This file defines the protocol messages and types used for communication
 * between the Wo compositor and Electron/Wayland clients.
 * 
 * These types are shared across the Rust compositor and TypeScript servers/clients.
 */

/**
 * IPC Protocol Magic Numbers (little-endian u32)
 */
export const MAGIC = {
  HELLO: 0x574F484C,         // "WOHL"
  FRAME: 0x574F4652,         // "WOFR"
  MOUSE_MOVE: 0x574F4D4D,    // "WOMM"
  MOUSE_BUTTON: 0x574F4D42,  // "WOMB"
  KEYBOARD: 0x574F4B42,      // "WOKB"
  SCROLL: 0x574F5343,        // "WOSC"
  ACTION: 0x574F4341,        // "WOAC"
  FOCUS_CHANGE: 0x574F4643,  // "WOFC"
  WINDOW_META: 0x574F574D,   // "WOWM"
  WINDOW_POS: 0x574F5750,    // "WOWP"
  SYSCALL: 0x574F5359,       // "WOSY"
  FRAME_ACK: 0x574F4641,     // "WOFA"
  SURFACE_BUFFER: 0x574F5342,  // "WOSB" — Wayland surface pixels from compositor
  SHM_BUFFER: 0x574F534D,      // "WOSM" — Wayland SHM surface sent to Electron via process FD
  DMABUF_FRAME: 0x574F4446,   // "WODF" — Wayland surface DMABUF FDs from compositor
  FORWARD_POINTER: 0x574F5045, // "WOPE" — forwarded pointer from web UI canvas
  FORWARD_KEYBOARD: 0x574F4B45, // "WOKE" — forwarded keyboard from web UI canvas
  FORWARD_RELATIVE_POINTER: 0x574F5245, // "WORE" — forwarded relative pointer (dx/dy)
  POINTER_LOCK_REQUEST: 0x574F504C, // "WOPL" — server-to-client pointer lock request
} as const;

/**
 * Mouse button codes
 */
export const MOUSE_BUTTON = {
  LEFT: 1,
  MIDDLE: 2,
  RIGHT: 3,
  BACK: 4,
  FORWARD: 5,
} as const;

/**
 * Input Events from Compositor → Client
 */
export interface MouseMoveEvent {
  type: 'mouseMove';
  x: number;
  y: number;
}

export interface MouseButtonEvent {
  type: 'mouseButton';
  button: number;
  pressed: boolean;
  time: number;
}

export interface KeyboardEvent {
  type: 'keyboard';
  key: number;
  pressed: boolean;
  time: number;
}

export interface ScrollEvent {
  type: 'scroll';
  vertical: number;
  horizontal: number;
  time: number;
}

export interface FocusChangeEvent {
  type: 'focusChange';
  window: string;
  focused: boolean;
}

export interface WindowMetadataEvent {
  type: 'windowMetadata';
  metadata: string;  // JSON string
}

export interface PointerLockRequestEvent {
  type: 'pointerLockRequest';
  window: string;
  lock: boolean;
}

export type InputEvent =
  | MouseMoveEvent
  | MouseButtonEvent
  | KeyboardEvent
  | ScrollEvent
  | FocusChangeEvent
  | WindowMetadataEvent
  | PointerLockRequestEvent;

/**
 * Compositor Actions from Client → Compositor
 */
export interface QuitAction {
  type: 'quit';
  code?: number;
}

export interface CustomAction {
  type: 'custom';
  action: string;
  payload?: Record<string, any>;
}

export type CompositorAction = QuitAction | CustomAction;

/**
 * Frame information for rendering
 */
export interface FrameInfo {
  seq: number;
  width: number;
  height: number;
  format: string;  // DRM fourcc as string
  planes: PlaneInfo[];
}

export interface PlaneInfo {
  offset: number;
  stride: number;
  modifier: bigint;
}

/**
 * DMABUF window frame information (metadata-only; FDs passed via ancillary data)
 */
export interface DmabufFrameInfo {
  name: string;
  width: number;
  height: number;
  format: number;  // DRM fourcc as u32
  numPlanes: number;
  planes: DmabufPlaneInfo[];
  fds: number[];  // File descriptors passed with this message
}

export interface DmabufPlaneInfo {
  offset: number;
  stride: number;
  modifier: bigint;
}

/**
 * Window configuration
 */
export interface WindowConfig {
  name: string;
  width: number;
  height: number;
  x?: number;
  y?: number;
  z_order?: number;
}

/**
 * Utility functions for working with the protocol
 */

export function numberToMagic(magic: number): string {
  return String.fromCharCode(
    magic & 0xff,
    (magic >> 8) & 0xff,
    (magic >> 16) & 0xff,
    (magic >> 24) & 0xff
  );
}

export function bufferToU32LE(buf: Buffer, offset: number = 0): number {
  return (
    buf[offset] |
    (buf[offset + 1] << 8) |
    (buf[offset + 2] << 16) |
    (buf[offset + 3] << 24)
  );
}

export function bufferToU64LE(buf: Buffer, offset: number = 0): bigint {
  const lo = BigInt(bufferToU32LE(buf, offset));
  const hi = BigInt(bufferToU32LE(buf, offset + 4));
  return (hi << BigInt(32)) | lo;
}

export function bufferToF64LE(buf: Buffer, offset: number = 0): number {
  return buf.readDoubleLE(offset);
}

export function bufferToI32LE(buf: Buffer, offset: number = 0): number {
  const val = bufferToU32LE(buf, offset);
  // Convert unsigned to signed
  return val > 0x7fffffff ? val - 0x100000000 : val;
}

export function u32ToBufferLE(val: number): Buffer {
  const buf = Buffer.alloc(4);
  buf.writeUInt32LE(val, 0);
  return buf;
}

export function u64ToBufferLE(val: bigint): Buffer {
  const buf = Buffer.alloc(8);
  const lo = Number(val & BigInt(0xffffffff));
  const hi = Number((val >> BigInt(32)) & BigInt(0xffffffff));
  buf.writeUInt32LE(lo, 0);
  buf.writeUInt32LE(hi, 4);
  return buf;
}

export function f64ToBufferLE(val: number): Buffer {
  const buf = Buffer.alloc(8);
  buf.writeDoubleLE(val, 0);
  return buf;
}

export function i32ToBufferLE(val: number): Buffer {
  const buf = Buffer.alloc(4);
  buf.writeInt32LE(val, 0);
  return buf;
}

export function stringToBuffer(str: string): Buffer {
  return Buffer.from(str, 'utf-8');
}

export function bufferToString(buf: Buffer, offset: number = 0, length?: number): string {
  return buf.toString('utf-8', offset, length !== undefined ? offset + length : undefined);
}
