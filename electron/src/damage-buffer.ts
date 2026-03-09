import { Socket } from 'net';

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
      Buffer.from(frame).copy(this.frame, 0, 0, this.frame.length);
      return;
    }

    const rowBytes = Math.min(frameStride, this.stride);
    for (let y = 0; y < this.height; y += 1) {
      const srcStart = y * frameStride;
      const dstStart = y * this.stride;
      Buffer.from(frame).copy(this.frame, dstStart, srcStart, srcStart + rowBytes);
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
      Buffer.from(patch.rgba).copy(this.frame, dstStart, srcStart, srcStart + rowBytes);
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

  constructor(socket: Socket, windowName: string, native: NativeDmabufModule) {
    this.socket = socket;
    this.windowName = windowName;
    this.native = native;
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

      const nameBuf = Buffer.from(this.windowName, 'utf8');
      const header = Buffer.alloc(4 + 4 + nameBuf.length + 8 + 4 + 4 + 4 + 4);
      let offset = 0;
      header.writeUInt32LE(MAGIC.FRAME, offset); offset += 4;
      header.writeUInt32LE(nameBuf.length, offset); offset += 4;
      nameBuf.copy(header, offset); offset += nameBuf.length;
      header.writeBigUInt64LE(this.seq, offset); offset += 8;
      header.writeUInt32LE(buffer.getWidth(), offset); offset += 4;
      header.writeUInt32LE(buffer.getHeight(), offset); offset += 4;
      header.writeUInt32LE(ARGB8888, offset); offset += 4;
      header.writeUInt32LE(1, offset);

      const plane = Buffer.alloc(16);
      plane.writeUInt32LE(imported.offset, 0);
      plane.writeUInt32LE(imported.stride, 4);
      plane.writeUInt32LE(imported.modifier_hi, 8);
      plane.writeUInt32LE(imported.modifier_lo, 12);

      this.socket.write(header);
      this.socket.write(plane);
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
