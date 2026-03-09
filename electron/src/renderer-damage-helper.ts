import type { DamageFramePayload, DamagePatch, DamageRect } from 'wo-types';

type SubmitFn = (payload: DamageFramePayload) => Promise<{ ok: boolean; seq?: string; reason?: string }>;

export interface DamageHelperOptions {
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
  captureFromCanvas(canvasLike: CanvasLike, dirtyRects?: DamageRect[]): void;
  flush(): Promise<{ ok: boolean; seq?: string; reason?: string; skipped?: boolean }>;
}

interface CanvasLike {
  width: number;
  height: number;
  getContext(type: '2d'): Canvas2DLike | null;
}

interface Canvas2DLike {
  getImageData(x: number, y: number, width: number, height: number): { data: Uint8ClampedArray };
}

export function createRendererDamageHelper(
  submit: SubmitFn,
  options: DamageHelperOptions,
): RendererDamageHelper {
  let width = options.width;
  let height = options.height;
  let stride = width * 4;
  let frame = new Uint8Array(stride * height);
  const maxRects = options.maxRects ?? 8;
  const fullFrameThreshold = options.fullFrameThreshold ?? 0.6;
  const dirtyRects: DamageRect[] = [];

  function resize(nextWidth: number, nextHeight: number): void {
    width = nextWidth;
    height = nextHeight;
    stride = width * 4;
    frame = new Uint8Array(stride * height);
    dirtyRects.length = 0;
    dirtyRects.push({ x: 0, y: 0, width, height });
  }

  function markDirty(rect: DamageRect): void {
    dirtyRects.push(clampRect(rect, width, height));
  }

  function updateFullFrame(rgba: Uint8Array, srcStride = width * 4): void {
    if (srcStride === stride && rgba.length >= frame.length) {
      frame.set(rgba.subarray(0, frame.length));
    } else {
      const rowBytes = Math.min(srcStride, stride);
      for (let y = 0; y < height; y += 1) {
        const srcStart = y * srcStride;
        const dstStart = y * stride;
        frame.set(rgba.subarray(srcStart, srcStart + rowBytes), dstStart);
      }
    }
    dirtyRects.length = 0;
    dirtyRects.push({ x: 0, y: 0, width, height });
  }

  function updatePatch(rect: DamageRect, rgba: Uint8Array, srcStride = rect.width * 4): void {
    const clamped = clampRect(rect, width, height);
    if (clamped.width <= 0 || clamped.height <= 0) {
      return;
    }
    const rowBytes = clamped.width * 4;
    const startX = clamped.x * 4;
    for (let row = 0; row < clamped.height; row += 1) {
      const srcStart = row * srcStride;
      const dstStart = (clamped.y + row) * stride + startX;
      frame.set(rgba.subarray(srcStart, srcStart + rowBytes), dstStart);
    }
    dirtyRects.push(clamped);
  }

  function captureFromCanvas(canvasLike: CanvasLike, rects?: DamageRect[]): void {
    if (!canvasLike || typeof canvasLike.getContext !== 'function') {
      throw new Error('Invalid canvas object');
    }
    if (canvasLike.width !== width || canvasLike.height !== height) {
      resize(canvasLike.width, canvasLike.height);
    }

    const context = canvasLike.getContext('2d');
    if (!context) {
      throw new Error('2D canvas context unavailable');
    }

    const targets = rects && rects.length > 0 ? rects : [{ x: 0, y: 0, width, height }];
    for (const target of targets) {
      const clamped = clampRect(target, width, height);
      if (clamped.width <= 0 || clamped.height <= 0) {
        continue;
      }
      const imageData = context.getImageData(clamped.x, clamped.y, clamped.width, clamped.height);
      const rgba = new Uint8Array(
        imageData.data.buffer,
        imageData.data.byteOffset,
        imageData.data.byteLength,
      );
      updatePatch(clamped, rgba, clamped.width * 4);
    }
  }

  async function flush(): Promise<{ ok: boolean; seq?: string; reason?: string; skipped?: boolean }> {
    if (dirtyRects.length === 0) {
      return { ok: true, skipped: true };
    }

    const merged = coalesceDamageRects(dirtyRects, maxRects);
    dirtyRects.length = 0;

    const damagedPixels = merged.reduce((acc, rect) => acc + rect.width * rect.height, 0);
    const totalPixels = width * height;

    if (damagedPixels / Math.max(1, totalPixels) >= fullFrameThreshold) {
      return submit({ width, height, fullFrame: frame, fullStride: stride });
    }

    const patches: DamagePatch[] = merged.map((rect) => ({
      rect,
      rgba: slicePatch(frame, stride, rect),
      stride: rect.width * 4,
    }));

    return submit({ width, height, patches });
  }

  return {
    resize,
    markDirty,
    updateFullFrame,
    updatePatch,
    captureFromCanvas,
    flush,
  };
}

function slicePatch(frame: Uint8Array, frameStride: number, rect: DamageRect): Uint8Array {
  const patchStride = rect.width * 4;
  const out = new Uint8Array(patchStride * rect.height);
  for (let row = 0; row < rect.height; row += 1) {
    const srcStart = (rect.y + row) * frameStride + rect.x * 4;
    const dstStart = row * patchStride;
    out.set(frame.subarray(srcStart, srcStart + patchStride), dstStart);
  }
  return out;
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
