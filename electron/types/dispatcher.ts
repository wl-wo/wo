import type { Compositor, CompositorActionType, CompositorActionConfig } from './types.js';

/**
 * A strongly-typed dispatcher for interacting with windows via the Compositor API.
 * Provides convenience methods for all supported compositor actions.
 */
export class InteractionDispatcher {
  private compositor: Compositor;

  constructor(compositor: Compositor) {
    this.compositor = compositor;
  }

  /**
   * Send a raw action to the compositor
   */
  public action(type: CompositorActionType, config: CompositorActionConfig): void {
    this.compositor.action(type, config);
  }

  /**
   * Focus a window by name
   */
  public focusWindow(windowName: string): void {
    this.action('focus', { window: windowName });
  }

  /**
   * Minimize a window by name
   */
  public minimizeWindow(windowName: string): void {
    this.action('minimize', { window: windowName });
  }

  /**
   * Maximize a window by name
   */
  public maximizeWindow(windowName: string): void {
    this.action('maximize', { window: windowName });
  }

  /**
   * Close a window by name
   */
  public closeWindow(windowName: string): void {
    this.action('close', { window: windowName });
  }

  /**
   * Resize a window by name
   */
  public resizeWindow(windowName: string, width: number, height: number): void {
    this.action('resize', { window: windowName, width, height });
  }

  /**
   * Move a window by name
   */
  public moveWindow(windowName: string, x: number, y: number): void {
    this.action('move', { window: windowName, x, y });
  }

  /**
   * Send pointer motion (mouse move) to a window
   */
  public pointerMotion(windowName: string, x: number, y: number): void {
    this.action('pointer_motion', { window: windowName, x, y });
  }

  /**
   * Send pointer button (mouse click) to a window
   * @param button Evdev button code (e.g. 272 for BTN_LEFT)
   */
  public pointerButton(windowName: string, x: number, y: number, button: number, pressed: boolean): void {
    this.action('pointer_button', { window: windowName, x, y, button, pressed });
  }

  /**
   * Send pointer scroll (mouse wheel) to a window
   */
  public pointerScroll(windowName: string, dx: number, dy: number): void {
    this.action('pointer_scroll', { window: windowName, dx, dy });
  }

  /**
   * Indicate the pointer has left the window
   */
  public pointerLeave(windowName: string): void {
    this.action('pointer_leave', { window: windowName });
  }
}
