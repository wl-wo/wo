/**
 * Wo Composer Input Event Receiver for Electron
 * 
 * This module provides utilities for Electron apps to receive input events
 * from the Wo compositor via the IPC protocol.
 * 
 * Usage in your Electron renderer process:
 * 
 *   const { WoInputReceiver } = require('./wo-input-receiver');
 *   const receiver = new WoInputReceiver();
 *   
 *   receiver.on('mouseMove', (x, y) => {
 *     console.log(`Mouse moved to: ${x}, ${y}`);
 *   });
 *   
 *   receiver.on('mouseButton', (button, pressed, time) => {
 *     console.log(`Button ${button} ${pressed ? 'pressed' : 'released'}`);
 *   });
 *   
 *   receiver.on('keyboard', (key, pressed, time) => {
 *     console.log(`Key ${key} ${pressed ? 'pressed' : 'released'}`);
 *   });
 *   
 *   receiver.on('scroll', (vertical, horizontal, time) => {
 *     console.log(`Scrolled V:${vertical} H:${horizontal}`);
 *   });
 *   
 *   receiver.startListening();
 * 
 * Or in the client app:
 * 
 *   // Access via window.__WO_INPUT_RECEIVER__
 *   window.__WO_INPUT_RECEIVER__.on('mouseMove', (x, y) => {
 *     // Handle mouse move
 *   });
 */

import { EventEmitter } from 'events';
import net from 'net';

const MAGIC_MOUSE_MOVE = 0x574F4D4D;    // "WOMM"
const MAGIC_MOUSE_BUTTON = 0x574F4D42;  // "WOMB"
const MAGIC_KEYBOARD = 0x574F4B42;      // "WOKB"
const MAGIC_SCROLL = 0x574F5343;        // "WOSC"

/**
 * Receives input events from the Wo compositor via IPC socket.
 * 
 * Events:
 *   - mouseMove(x, y)                      - Pointer moved
 *   - mouseButton(button, pressed, time)   - Mouse button event
 *   - keyboard(key, pressed, time)         - Keyboard event
 *   - scroll(vertical, horizontal, time)   - Scroll wheel event
 *   - connected()                          - Connected to compositor
 *   - disconnected(reason)                 - Disconnected from compositor
 *   - error(error)                         - Error occurred
 */
class WoInputReceiver extends EventEmitter {
  private socket: net.Socket | null;
  private ipcSocket: string;
  private windowName: string;
  private buffer: Buffer;
  private connected: boolean;

  constructor(ipcSocket?: string, windowName?: string) {
    super();
    this.socket = null;
    this.ipcSocket = ipcSocket || process.env.WO_IPC_SOCKET!;
    this.windowName = windowName || process.env.WO_WINDOW_NAME || 'unknown';
    this.buffer = Buffer.alloc(0);
    this.connected = false;
  }

  /**
   * Start listening for input events from the Wo compositor.
   * This is typically called automatically when connecting to the compositor.
   */
  startListening() {
    if (this.socket) {
      this.socket.removeAllListeners();
      this.socket.destroy();
    }

    // Create a new socket connection just for receiving input events
    // This reuses the DMABUF connection through parameter passing
    this.socket = net.createConnection(this.ipcSocket);

    this.socket.on('connect', () => {
      this.connected = true;
      console.log('[Wo] Input event receiver connected');
      this.emit('connected');
    });

    this.socket.on('data', (chunk: Buffer) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.processEventBuffer();
    });

    this.socket.on('error', (error: Error) => {
      console.error('[Wo] Input receiver error:', error);
      this.emit('error', error);
    });

    this.socket.on('close', () => {
      this.connected = false;
      console.log('[Wo] Input event receiver disconnected');
      this.emit('disconnected');
    });
  }

  /**
   * Process all complete events in the buffer.
   * Events are variable-length, so we parse them as they arrive.
   */
  processEventBuffer() {
    while (this.buffer.length >= 4) {
      const magic = this.buffer.readUInt32LE(0);

      let eventSize = 0;
      let parsed = false;

      // Mouse move: 4 (magic) + 8 (x) + 8 (y) = 20 bytes
      if (magic === MAGIC_MOUSE_MOVE) { // "WOMM"
        if (this.buffer.length >= 20) {
          const x = this.buffer.readDoubleLE(4);
          const y = this.buffer.readDoubleLE(12);
          this.emit('mouseMove', x, y);
          this.buffer = this.buffer.slice(20);
          parsed = true;
        }
      }
      // Mouse button: 4 (magic) + 4 (button) + 4 (pressed) + 4 (time) = 16 bytes
      else if (magic === MAGIC_MOUSE_BUTTON) { // "WOMB"
        if (this.buffer.length >= 16) {
          const button = this.buffer.readUInt32LE(4);
          const pressed = this.buffer.readUInt32LE(8) !== 0;
          const time = this.buffer.readUInt32LE(12);
          this.emit('mouseButton', button, pressed, time);
          this.buffer = this.buffer.slice(16);
          parsed = true;
        }
      }
      // Keyboard: 4 (magic) + 4 (key) + 4 (pressed) + 4 (time) = 16 bytes
      else if (magic === MAGIC_KEYBOARD) { // "WOKB"
        if (this.buffer.length >= 16) {
          const key = this.buffer.readUInt32LE(4);
          const pressed = this.buffer.readUInt32LE(8) !== 0;
          const time = this.buffer.readUInt32LE(12);
          this.emit('keyboard', key, pressed, time);
          this.buffer = this.buffer.slice(16);
          parsed = true;
        }
      }
      // Scroll: 4 (magic) + 4 (vertical) + 4 (horizontal) + 4 (time) = 16 bytes
      else if (magic === MAGIC_SCROLL) { // "WOSC"
        if (this.buffer.length >= 16) {
          const vertical = this.buffer.readInt32LE(4);
          const horizontal = this.buffer.readInt32LE(8);
          const time = this.buffer.readUInt32LE(12);
          this.emit('scroll', vertical, horizontal, time);
          this.buffer = this.buffer.slice(16);
          parsed = true;
        }
      }
      else {
        // Unknown magic, skip this byte and try again
        console.warn('[Wo] Unknown message magic: 0x' + magic.toString(16));
        this.buffer = this.buffer.slice(1);
        parsed = true;
      }

      // If we couldn't parse a complete event, wait for more data
      if (!parsed) {
        break;
      }
    }
  }

  /**
   * Stop listening for input events.
   */
  stop() {
    if (this.socket) {
      this.socket.destroy();
      this.socket = null;
    }
  }

  /**
   * Destroy the receiver and clean up resources.
   */
  destroy() {
    this.stop();
    this.removeAllListeners();
  }
}

export { WoInputReceiver };

