import { Socket } from 'net';
import { writeSync } from 'fs';

import { MAGIC } from './protocol.js';

export interface DamageRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface DamagePatch {
  rect: DamageRect;
  rgba: Uint8Array;
  stride?: number;
}

export interface DamageFramePayload {
  width: number;
  height: number;
  fullFrame?: Uint8Array;
  fullStride?: number;
  patches?: DamagePatch[];
}

export class DamageRegionTracker {
  private rects: DamageRect[] = [];

  add(rect: DamageRect): void {
    this.rects.push(rect);
  }

  addAll(rects: DamageRect[]): void {
    for (const rect of rects) {
      this.add(rect);
    }
  }

  take(maxRects = 8): DamageRect[] {
    const merged = coalesceDamageRects(this.rects, maxRects);
    this.rects = [];
    return merged;
  }
}

type ImportedDmabuf = {
  fd: number;
  offset: number;
  stride: number;
  modifier_hi: number;
  modifier_lo: number;
  token: number;
};

type NativeDmabufModule = {
  init: (drmDevice?: string) => void;
  importRgba: (buf: Buffer, width: number, height: number, stride: number) => ImportedDmabuf;
  releaseBuffer: (token: number) => void;
  sendFd: (socketFd: number, dmabufFd: number) => void;
  recvFd: (socketFd: number) => number;
  mmapFd: (fd: number, size: number) => Buffer;
  copyMmapToSab: (fd: number, sab: SharedArrayBuffer, size: number, srcOff?: number, dstOff?: number) => void;
  copyMmapDamageToSab: (fd: number, sab: SharedArrayBuffer, stride: number, rects: Array<{y: number; h: number}>) => void;
  importDmabufTexture: (windowName: string, fd: number, width: number, height: number, format: number) => { texture: number; width: number; height: number };
  getTexture: (windowName: string) => { texture: number; width: number; height: number } | null;
  releaseTexture: (windowName: string) => void;
};

const ARGB8888 = fourccCode('A', 'R', '2', '4');

export class DamageBuffer {
  private width: number;
  private height: number;
  private stride: number;
  private frame: Buffer;

  constructor(width: number, height: number, stride: number = width * 4) {
    this.width = width;
    this.height = height;
    this.stride = stride;
    this.frame = Buffer.alloc(this.stride * this.height);
  }

  getWidth(): number {
    return this.width;
  }

  getHeight(): number {
    return this.height;
  }

  getStride(): number {
    return this.stride;
  }

  getFrameBuffer(): Buffer {
    return this.frame;
  }

  reset(width: number, height: number, stride: number = width * 4): void {
    this.width = width;
    this.height = height;
    this.stride = stride;
    this.frame = Buffer.alloc(this.stride * this.height);
  }

  applyFullFrame(frame: Uint8Array, frameStride: number = this.width * 4): void {
    if (frameStride === this.stride) {
      // Fast path: direct typed-array copy, no intermediate Buffer wrapper
      this.frame.set(new Uint8Array(frame.buffer, frame.byteOffset, this.frame.length));
      return;
    }

    const rowBytes = Math.min(frameStride, this.stride);
    for (let y = 0; y < this.height; y += 1) {
      const srcStart = y * frameStride;
      const dstStart = y * this.stride;
      this.frame.set(
        new Uint8Array(frame.buffer, frame.byteOffset + srcStart, rowBytes),
        dstStart,
      );
    }
  }

  applyPatch(patch: DamagePatch): void {
    const rect = clampRect(patch.rect, this.width, this.height);
    if (rect.width <= 0 || rect.height <= 0) {
      return;
    }

    const patchStride = patch.stride ?? rect.width * 4;
    const dstStartX = rect.x * 4;
    const rowBytes = rect.width * 4;

    for (let row = 0; row < rect.height; row += 1) {
      const srcStart = row * patchStride;
      const dstStart = (rect.y + row) * this.stride + dstStartX;
      this.frame.set(
        new Uint8Array(patch.rgba.buffer, patch.rgba.byteOffset + srcStart, rowBytes),
        dstStart,
      );
    }
  }

  applyPatches(patches: DamagePatch[]): void {
    for (const patch of patches) {
      this.applyPatch(patch);
    }
  }
}

export class WoDmabufSender {
  private readonly socket: Socket;
  private readonly windowName: string;
  private readonly native: NativeDmabufModule;
  private seq = 1n;
  // Pre-allocated wire buffer: header (variable) + plane (16 bytes).
  // Layout: MAGIC(4) + nameLen(4) + name(N) + seq(8) + w(4) + h(4) + fmt(4) + planes(4) + plane(16)
  private readonly wireBuf: Buffer;
  private readonly nameLen: number;
  private readonly seqOffset: number;
  private readonly widthOffset: number;
  private readonly heightOffset: number;

  constructor(socket: Socket, windowName: string, native: NativeDmabufModule) {
    this.socket = socket;
    this.windowName = windowName;
    this.native = native;

    const nameBuf = Buffer.from(windowName, 'utf8');
    this.nameLen = nameBuf.length;
    // header = 4+4+nameLen+8+4+4+4+4 = 32+nameLen, plus 16 bytes for plane
    const totalLen = 32 + this.nameLen + 16;
    this.wireBuf = Buffer.alloc(totalLen);

    // Static fields that never change
    let off = 0;
    this.wireBuf.writeUInt32LE(MAGIC.FRAME, off); off += 4;
    this.wireBuf.writeUInt32LE(this.nameLen, off); off += 4;
    nameBuf.copy(this.wireBuf, off); off += this.nameLen;
    this.seqOffset = off; off += 8;
    this.widthOffset = off; off += 4;
    this.heightOffset = off; off += 4;
    this.wireBuf.writeUInt32LE(ARGB8888, off); off += 4;
    this.wireBuf.writeUInt32LE(1, off); // planes count = 1
  }

  send(buffer: DamageBuffer): bigint {
    const imported = this.native.importRgba(
      buffer.getFrameBuffer(),
      buffer.getWidth(),
      buffer.getHeight(),
      buffer.getStride(),
    );

    try {
      const socketFd = getSocketFd(this.socket);

      // Write per-frame fields into the pre-allocated buffer
      this.wireBuf.writeBigUInt64LE(this.seq, this.seqOffset);
      this.wireBuf.writeUInt32LE(buffer.getWidth(), this.widthOffset);
      this.wireBuf.writeUInt32LE(buffer.getHeight(), this.heightOffset);

      // Plane data (last 16 bytes of wireBuf)
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

export function applyDamagePayload(buffer: DamageBuffer, payload: DamageFramePayload): void {
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

export function loadNativeDmabufModule(requireFn: NodeRequire, nativePath: string): NativeDmabufModule {
  const native = requireFn(nativePath) as NativeDmabufModule;
  return native;
}

function getSocketFd(socket: Socket): number {
  const fd = (socket as any)?._handle?.fd;
  if (typeof fd !== 'number' || !Number.isFinite(fd) || fd < 0) {
    throw new Error('Unable to read underlying socket file descriptor');
  }
  return fd;
}

function clampRect(rect: DamageRect, width: number, height: number): DamageRect {
  const x = Math.max(0, Math.min(width, rect.x));
  const y = Math.max(0, Math.min(height, rect.y));
  const right = Math.max(x, Math.min(width, rect.x + rect.width));
  const bottom = Math.max(y, Math.min(height, rect.y + rect.height));
  return { x, y, width: Math.max(0, right - x), height: Math.max(0, bottom - y) };
}

function coalesceDamageRects(input: DamageRect[], maxRects: number): DamageRect[] {
  if (input.length <= 1) {
    return input.slice();
  }

  const rects = input
    .map((r) => ({ ...r }))
    .filter((r) => r.width > 0 && r.height > 0);

  const out: DamageRect[] = [];
  while (rects.length > 0) {
    let current = rects.pop()!;
    let mergedAny = true;

    while (mergedAny) {
      mergedAny = false;
      for (let i = rects.length - 1; i >= 0; i -= 1) {
        const candidate = rects[i];
        if (intersectsOrTouches(current, candidate)) {
          current = unionRect(current, candidate);
          rects.splice(i, 1);
          mergedAny = true;
        }
      }
    }

    out.push(current);
  }

  if (out.length <= maxRects) {
    return out;
  }

  return [out.reduce((acc, rect) => unionRect(acc, rect))];
}

function intersectsOrTouches(a: DamageRect, b: DamageRect): boolean {
  return !(
    a.x + a.width < b.x ||
    b.x + b.width < a.x ||
    a.y + a.height < b.y ||
    b.y + b.height < a.y
  );
}

function unionRect(a: DamageRect, b: DamageRect): DamageRect {
  const x1 = Math.min(a.x, b.x);
  const y1 = Math.min(a.y, b.y);
  const x2 = Math.max(a.x + a.width, b.x + b.width);
  const y2 = Math.max(a.y + a.height, b.y + b.height);
  return { x: x1, y: y1, width: x2 - x1, height: y2 - y1 };
}

function fourccCode(a: string, b: string, c: string, d: string): number {
  return (
    a.charCodeAt(0) |
    (b.charCodeAt(0) << 8) |
    (c.charCodeAt(0) << 16) |
    (d.charCodeAt(0) << 24)
  ) >>> 0;
}
