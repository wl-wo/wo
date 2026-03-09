# wo-types

Type definitions for the Wo Wayland compositor Electron integration.

This package provides fully-typed interfaces for the compositor APIs exposed to Electron windows running within the Wo compositor.

## Installation

```bash
bun install wo-types
# or
npm install wo-types
```

## Usage

### In Preload Scripts

```typescript
import type { CompositorAPI, WoClientAPI } from 'wo-types';

// These are already exposed as window.compositor and window.woClient
const compositorAPI: CompositorAPI = window.compositor;
const clientAPI: WoClientAPI = window.woClient;
```

### In React Components

```typescript
import type { WoWindow, WindowMetadataEvent } from 'wo-types';
import { useEffect, useState } from 'react';

function MyComponent() {
  const [windows, setWindows] = useState<WoWindow[]>([]);
  const [metadata, setMetadata] = useState<WindowMetadataEvent | null>(null);

  useEffect(() => {
    // Subscribe to window updates
    const unsubWindows = window.woClient?.onWindows((windowList) => {
      setWindows(windowList);
    });

    // Subscribe to metadata updates
    const unsubMetadata = window.compositor?.onWindowMetadata((meta) => {
      setMetadata(meta);
    });

    return () => {
      unsubWindows?.();
      unsubMetadata?.();
    };
  }, []);

  return (
    <div>
      <p>Active windows: {metadata?.count ?? 0}</p>
    </div>
  );
}
```

## API Reference

### `CompositorAPI`

Exposed as `window.compositor` in Electron windows.

#### Methods

- `action(name: string, payload?: Record<string, unknown>): Promise<unknown>`
  - Send an action to the compositor
  
- `actionSync(name: string, payload?: Record<string, unknown>): void`
  - Send an action synchronously (fire and forget)
  
- `quit(code?: number): Promise<unknown>`
  - Request the compositor to quit
  
- `reload(): Promise<unknown>`
  - Reload the current window
  
- `toggleDevTools(): Promise<unknown>`
  - Toggle DevTools for the current window
  
- `onFocusChange(callback: (window: string, focused: boolean) => void): () => void`
  - Register focus change notification callback
  
- `onWindowMetadata(callback: (metadata: WindowMetadataEvent) => void): () => void`
  - Register window metadata update callback
  
- `syscall(type: string, params: Record<string, unknown>): Promise<unknown>`
  - Execute a syscall in the compositor

### `WoClientAPI`

Exposed as `window.woClient` in Electron windows.

#### Methods

- `getWindows(): Promise<WoWindow[]>`
  - Get list of all windows
  
- `onWindows(callback: (windows: WoWindow[]) => void): () => void`
  - Register window list update callback
  
- `sendAction(action: string): void`
  - Send a keybind action to the compositor

## Types

### `WindowMetadata`

Describes a Wayland surface or application window.

```typescript
interface WindowMetadata {
  name: string;
  title?: string;
  width: number;
  height: number;
  x: number;
  y: number;
  z_order: number;
  fps: number;
  focused?: boolean;
  focusable: boolean;
  interactive: boolean;
}
```

### `WoWindow`

Window information returned by the client API.

```typescript
interface WoWindow {
  name: string;
  title?: string;
  x: number;
  y: number;
  width: number;
  height: number;
  z_order: number;
  fps: number;
  focused: boolean;
}
```

### `WindowMetadataEvent`

Event data for window metadata updates.

```typescript
interface WindowMetadataEvent {
  windows: WindowMetadata[];
  count: number;
}
```

### `FocusChangeEvent`

Event data for focus change notifications.

```typescript
interface FocusChangeEvent {
  window: string;
  focused: boolean;
}
```

### `S2CAction`

Enum of server-to-client actions.

```typescript
enum S2CAction {
  WindowMetadataUpdate = 'window-metadata-update',
  FocusChange = 'focus-change',
  SyscallResponse = 'syscall-response',
  InputEvent = 'input-event',
}
```

## License

MIT
