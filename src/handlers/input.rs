use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::keyboard::FilterResult,
    utils::SERIAL_COUNTER,
};
use tracing::info;

use crate::{electron::ElectronInputEvent, handlers::xdg_shell::GrabState, state::WoState};

fn evdev_function_key_to_vt(evdev_key: u32) -> Option<u32> {
    match evdev_key {
        59..=68 => Some(evdev_key - 58), // F1..F10 -> VT1..VT10
        87 => Some(11),                  // F11 -> VT11
        88 => Some(12),                  // F12 -> VT12
        _ => None,
    }
}

impl WoState {
    /// Dispatch a generic input event to the appropriate handler.
    pub fn process_input_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event } => self.on_keyboard::<B>(event),
            InputEvent::PointerMotion { event } => self.on_pointer_motion::<B>(event),
            InputEvent::PointerMotionAbsolute { event } => self.on_pointer_motion_abs::<B>(event),
            InputEvent::PointerButton { event } => self.on_pointer_button::<B>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<B>(event),
            _ => {}
        }
    }

    /// Determine which Electron window (if any) is under the given position.
    fn window_under(&self, pos: (f64, f64)) -> Option<String> {
        // Windows are drawn in z-order, so we check them in reverse z-order
        // to find the topmost one under the cursor.
        let mut windows: Vec<_> = self
            .config
            .windows
            .iter()
            .chain(self.config.root.iter())
            .collect();
        windows.sort_by_key(|w| std::cmp::Reverse(w.z_order));

        for win in windows {
            let x1 = win.x as f64;
            let y1 = win.y as f64;
            let x2 = x1 + win.width as f64;
            let y2 = y1 + win.height as f64;
            // Skip windows that are explicitly unmapped by the compositor
            // (Electron-managed windows signal this via `window_mapped[name] = false`).
            if let Some(mapped) = self.window_mapped.get(&win.name) {
                if !*mapped {
                    continue;
                }
            }

            if pos.0 >= x1 && pos.0 < x2 && pos.1 >= y1 && pos.1 < y2 {
                return Some(win.name.clone());
            }
        }
        None
    }

    /// Get the coordinates of a window relative to the window's top-left.
    fn local_window_coords(&self, window_name: &str, global_pos: (f64, f64)) -> Option<(f64, f64)> {
        self.config
            .windows
            .iter()
            .chain(self.config.root.iter())
            .find(|w| w.name == window_name)
            .map(|w| (global_pos.0 - w.x as f64, global_pos.1 - w.y as f64))
    }

    fn on_keyboard<B: InputBackend>(&mut self, event: B::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        let press = event.state() == KeyState::Pressed;
        let key_code = event.key_code();

        // Smithay's libinput backend stores XKB keycodes (evdev + 8).
        // The Electron keycode map uses raw evdev scancodes, so subtract
        // the XKB offset before forwarding.
        let evdev_key = u32::from(key_code).saturating_sub(8);

        // Check if a client is inhibiting compositor keyboard shortcuts
        // (e.g. fullscreen games requesting all key events).
        let shortcuts_inhibited = {
            use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
            self.seat.keyboard_shortcuts_inhibited()
        };

        // Use non-blocking send to avoid input stalls.
        let wayland_has_focus = self
            .seat
            .get_keyboard()
            .map(|kb| kb.current_focus().is_some())
            .unwrap_or(false);

        if !wayland_has_focus {
            if let Some(ref focused_win) = self.keyboard_window_focus {
                if let Some(ref ipc) = self.electron_ipc {
                    if let Some(client) = ipc.clients.lock().unwrap().get(focused_win).cloned() {
                        // Non-blocking send.
                        let _ = client.try_send_input_event(&ElectronInputEvent::Keyboard {
                            key: evdev_key,
                            pressed: press,
                            time,
                        });
                    }
                }
            }
        }

        // Always dispatch through Smithay's keyboard handler (forwards to focused Wayland surface).
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.input::<(), _>(
            self,
            key_code,
            event.state(),
            serial,
            time,
            |state, modifiers, _handle| {
                // When keyboard shortcuts are inhibited, forward all keys to the
                // focused client without intercepting compositor shortcuts.
                if shortcuts_inhibited {
                    return FilterResult::Forward;
                }

                if press && state.can_switch_vt {
                    if let Some(vt) = evdev_function_key_to_vt(evdev_key) {
                        // Accept Ctrl+Alt+Fn and Meta+Alt+Fn.
                        if modifiers.alt && (modifiers.ctrl || modifiers.logo) {
                            state.pending_vt_switch = Some(vt);
                            info!(vt, "VT switch requested from keyboard shortcut");
                            return FilterResult::Intercept(());
                        }
                    }
                }

                let is_backspace = evdev_key == 14;
                if press && modifiers.ctrl && modifiers.alt && is_backspace {
                    info!("Ctrl+Alt+Backspace pressed, quitting");
                    state.running = false;
                    return FilterResult::Intercept(());
                }
                FilterResult::Forward
            },
        );
    }

    fn on_pointer_motion<B: InputBackend>(&mut self, event: B::PointerMotionEvent) {
        let delta = (event.delta_x(), event.delta_y());

        let current = self.pointer_location;
        let new_pos: smithay::utils::Point<f64, smithay::utils::Logical> = (
            (current.x + delta.0).clamp(0.0, self.output_size.0 as f64),
            (current.y + delta.1).clamp(0.0, self.output_size.1 as f64),
        )
            .into();

        // Hardware is the sole source of truth for pointer position.
        self.pointer_location = new_pos;

        // Process active grab (interactive move/resize) before normal dispatch
        if let Some(ref grab) = self.grab_state.clone() {
            match grab {
                GrabState::Move {
                    window,
                    initial_window_location,
                    initial_pointer,
                } => {
                    let dx = self.pointer_location.x - initial_pointer.x;
                    let dy = self.pointer_location.y - initial_pointer.y;
                    let new_loc = (
                        initial_window_location.x + dx as i32,
                        initial_window_location.y + dy as i32,
                    );
                    self.space.map_element(window.clone(), new_loc, false);
                }
                GrabState::Resize {
                    surface,
                    window,
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_pointer,
                } => {
                    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;
                    let dx = (self.pointer_location.x - initial_pointer.x) as i32;
                    let dy = (self.pointer_location.y - initial_pointer.y) as i32;

                    let (mut new_w, mut new_h) = *initial_window_size;
                    let (mut new_x, mut new_y) =
                        (initial_window_location.x, initial_window_location.y);

                    match *edges {
                        ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => {
                            new_w = (new_w + dx).max(1);
                        }
                        ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
                            new_w = (new_w - dx).max(1);
                            new_x += dx;
                        }
                        _ => {}
                    }
                    match *edges {
                        ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => {
                            new_h = (new_h + dy).max(1);
                        }
                        ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => {
                            new_h = (new_h - dy).max(1);
                            new_y += dy;
                        }
                        _ => {}
                    }

                    // Respect client min/max size constraints.
                    let (clamped_w, clamped_h) = self.clamp_to_size_hints(window, new_w, new_h);

                    surface.with_pending_state(|s| {
                        s.size = Some((clamped_w, clamped_h).into());
                    });
                    surface.send_configure();
                    self.space
                        .map_element(window.clone(), (new_x, new_y), false);
                }
            }
            return;
        }

        self.dispatch_pointer_to_targets(event.time_msec());
    }

    fn on_pointer_motion_abs<B: InputBackend>(&mut self, event: B::PointerMotionAbsoluteEvent) {
        let new_pos: smithay::utils::Point<f64, smithay::utils::Logical> = event
            .position_transformed((self.output_size.0 as i32, self.output_size.1 as i32).into())
            .into();

        // Hardware is the sole source of truth for pointer position.
        self.pointer_location = new_pos;

        // Process active grab (interactive move/resize) before normal dispatch
        if let Some(ref grab) = self.grab_state.clone() {
            match grab {
                GrabState::Move {
                    window,
                    initial_window_location,
                    initial_pointer,
                } => {
                    let dx = self.pointer_location.x - initial_pointer.x;
                    let dy = self.pointer_location.y - initial_pointer.y;
                    let new_loc = (
                        initial_window_location.x + dx as i32,
                        initial_window_location.y + dy as i32,
                    );
                    self.space.map_element(window.clone(), new_loc, false);
                }
                GrabState::Resize {
                    surface,
                    window,
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_pointer,
                } => {
                    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;
                    let dx = (self.pointer_location.x - initial_pointer.x) as i32;
                    let dy = (self.pointer_location.y - initial_pointer.y) as i32;

                    let (mut new_w, mut new_h) = *initial_window_size;
                    let (mut new_x, mut new_y) =
                        (initial_window_location.x, initial_window_location.y);

                    match *edges {
                        ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => {
                            new_w = (new_w + dx).max(1);
                        }
                        ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
                            new_w = (new_w - dx).max(1);
                            new_x += dx;
                        }
                        _ => {}
                    }
                    match *edges {
                        ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => {
                            new_h = (new_h + dy).max(1);
                        }
                        ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => {
                            new_h = (new_h - dy).max(1);
                            new_y += dy;
                        }
                        _ => {}
                    }

                    // Respect client min/max size constraints.
                    let (clamped_w, clamped_h) = self.clamp_to_size_hints(window, new_w, new_h);

                    surface.with_pending_state(|s| {
                        s.size = Some((clamped_w, clamped_h).into());
                    });
                    surface.send_configure();
                    self.space
                        .map_element(window.clone(), (new_x, new_y), false);
                }
            }
            return;
        }

        self.dispatch_pointer_to_targets(event.time_msec());
    }

    /// Route the current pointer location to Electron windows (IPC) and/or
    /// native Wayland/X11 windows (Smithay pointer.motion). Called from both
    /// relative and absolute pointer motion handlers.
    fn dispatch_pointer_to_targets(&mut self, time: u32) {
        use smithay::{
            desktop::WindowSurfaceType,
            input::pointer::MotionEvent,
            utils::{Logical, Point},
        };

        let pos = (self.pointer_location.x, self.pointer_location.y);

        // Route to Electron-managed window if cursor is over one
        let under_electron = self.window_under(pos);
        if self.pointer_window_focus != under_electron {
            self.pointer_window_focus = under_electron.clone();
        }
        if let Some(ref win_name) = under_electron {
            if let Some(ref ipc) = self.electron_ipc {
                if let Some(local) = self.local_window_coords(win_name, pos) {
                    if let Some(client) = ipc.clients.lock().unwrap().get(win_name).cloned() {
                        let _ = client.try_send_input_event(&ElectronInputEvent::MouseMove {
                            x: local.0,
                            y: local.1,
                        });
                    }
                }
            }
        }

        let pointer = match self.seat.get_pointer() {
            Some(p) => p,
            None => return,
        };
        let serial = SERIAL_COUNTER.next_serial();

        // When the cursor is over an Electron-managed window, the client
        // (comraw) owns event dispatching.  The compositor sends the position
        // via IPC (above) but does NOT call pointer.motion() — the client's
        // forwarded pointer_motion action handles that.
        if under_electron.is_some() && self.electron_ipc.is_some() {
            return;
        }

        // Not over an Electron window — dispatch directly to native windows.
        let pos_logical: Point<f64, Logical> = self.pointer_location;
        let under_native = self.space.element_under(pos_logical);
        let focus = under_native.and_then(|(window, _loc)| {
            let win_loc = self.space.element_location(&window)?;
            let local_x = self.pointer_location.x - win_loc.x as f64;
            let local_y = self.pointer_location.y - win_loc.y as f64;
            window
                .surface_under(Point::<f64, Logical>::from((local_x, local_y)), WindowSurfaceType::ALL)
                .map(|(s, offset)| {
                    let surface_global: Point<f64, Logical> = (
                        win_loc.x as f64 + offset.x as f64,
                        win_loc.y as f64 + offset.y as f64,
                    ).into();
                    (s, surface_global)
                })
        });
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_button<B: InputBackend>(&mut self, event: B::PointerButtonEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        let button_state = event.state();
        let button_code = event.button_code();

        // End any active grab on button release
        if button_state == ButtonState::Released && self.grab_state.is_some() {
            self.grab_state = None;
            return;
        }

        // In nested mode, pointer button events are routed through Electron/comraw.
        if self.electron_ipc.is_some() {
            let under_window =
                self.window_under((self.pointer_location.x, self.pointer_location.y));

            if let Some(ref win_name) = under_window {
                if button_state == ButtonState::Pressed {
                    // Only set keyboard focus to the Electron window and clear
                    // Wayland focus if we don't already have a native window
                    // focused via the forwarded path.  If the user clicked on a
                    // native window canvas inside Electron/comraw, the forwarded
                    // pointer_button action will arrive shortly and set the
                    // correct Wayland surface focus.  Clearing it here would
                    // cause focus to bounce (clear → set → clear → set).
                    let has_native_focus = self.seat.get_keyboard()
                        .map(|kb| kb.current_focus().is_some())
                        .unwrap_or(false);
                    if !has_native_focus {
                        self.keyboard_window_focus = Some(win_name.clone());
                    }
                }

                if let Some(ref ipc) = self.electron_ipc {
                    if let Some(client) = ipc.clients.lock().unwrap().get(win_name).cloned() {
                        // Non-blocking send.
                        let _ = client.try_send_input_event(&ElectronInputEvent::MouseButton {
                            button: button_code,
                            pressed: button_state == ButtonState::Pressed,
                            time,
                        });
                    }
                }
            } else {
                // Fall back to native window hit-testing.
                if button_state == ButtonState::Pressed {
                    use smithay::utils::{Logical, Point};
                    let pos: Point<f64, Logical> = self.pointer_location;
                    let under_surface = self.space.element_under(pos);

                    if let Some((window, _surface_loc)) = under_surface {
                        let surface_opt = if let Some(x11) = window.x11_surface() {
                            x11.wl_surface()
                        } else {
                            window.toplevel().map(|t| t.wl_surface().clone())
                        };

                        if let Some(surface) = surface_opt {
                            let keyboard = self.seat.get_keyboard().unwrap();
                            keyboard.set_focus(self, Some(surface.clone()), serial);
                            self.keyboard_window_focus = None;

                            let pointer = self.seat.get_pointer().unwrap();
                            pointer.button(
                                self,
                                &smithay::input::pointer::ButtonEvent {
                                    button: button_code,
                                    state: button_state,
                                    serial,
                                    time,
                                },
                            );
                        }
                    } else {
                        // Clicked empty space: clear focus.
                        self.keyboard_window_focus = None;
                        let keyboard = self.seat.get_keyboard().unwrap();
                        keyboard.set_focus(self, None, serial);
                    }
                }
            }
            return;
        }
    }

    fn on_pointer_axis<B: InputBackend>(&mut self, event: B::PointerAxisEvent) {
        let time = event.time_msec();

        // In nested mode, scroll events are routed through Electron/comraw.
        if self.electron_ipc.is_some() {
            let under_window =
                self.window_under((self.pointer_location.x, self.pointer_location.y));

            if let Some(ref win_name) = under_window {
                if let Some(ref ipc) = self.electron_ipc {
                    let mut vertical = 0i32;
                    let mut horizontal = 0i32;

                    if let Some(v) = event.amount_v120(Axis::Vertical) {
                        vertical = v as i32;
                    } else if let Some(v) = event.amount(Axis::Vertical) {
                        vertical = (v * 15.0) as i32;
                    }
                    if let Some(h) = event.amount_v120(Axis::Horizontal) {
                        horizontal = h as i32;
                    } else if let Some(h) = event.amount(Axis::Horizontal) {
                        horizontal = (h * 15.0) as i32;
                    }

                    if vertical != 0 || horizontal != 0 {
                        if let Some(client) = ipc.clients.lock().unwrap().get(win_name).cloned() {
                            // Non-blocking send.
                            let _ = client.try_send_input_event(&ElectronInputEvent::Scroll {
                                vertical,
                                horizontal,
                                time,
                            });
                        }
                    }
                }
            }
            return;
        }
    }

    /// Forward a pointer motion event from the client (comraw/Electron) to the
    /// Wayland surface under the given window-local coordinates.
    ///
    /// The client controls event dispatch to windows it renders.  However, the
    /// compositor remains the sole authority on `pointer_location` (hardware
    /// input) — this function does NOT overwrite it.
    pub fn handle_forwarded_pointer_event(&mut self, window_name: &str, x: f64, y: f64) {
        use smithay::{
            desktop::WindowSurfaceType,
            input::pointer::MotionEvent,
            utils::{Logical, Point},
        };

        let window = match self.find_wayland_window(window_name) {
            Some(w) => w,
            None => return,
        };

        let pointer = match self.seat.get_pointer() {
            Some(p) => p,
            None => return,
        };
        let serial = SERIAL_COUNTER.next_serial();

        let win_loc = self.space.element_location(&window).unwrap_or_default();

        // Compute a synthetic global location from the client-provided
        // surface-local coords.  This is used ONLY for the MotionEvent so
        // Smithay derives the correct wl_pointer surface-local coordinates.
        // pointer_location itself is NOT modified.
        let dispatch_location: Point<f64, Logical> = (
            win_loc.x as f64 + x,
            win_loc.y as f64 + y,
        ).into();

        let focus = window
            .surface_under(Point::<f64, Logical>::from((x, y)), WindowSurfaceType::ALL)
            .map(|(s, offset)| {
                let focus_pos: Point<f64, Logical> = (
                    win_loc.x as f64 + offset.x as f64,
                    win_loc.y as f64 + offset.y as f64,
                ).into();
                (s, focus_pos)
            });

        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: dispatch_location,
                serial,
                time: 0,
            },
        );
        pointer.frame(self);
    }

    /// Forward a keyboard event received from the Electron web UI to the
    /// Wayland surface corresponding to the given window name.
    pub fn handle_forwarded_keyboard_event(
        &mut self,
        window_name: &str,
        evdev_key: u32,
        pressed: bool,
        time: u32,
    ) {
        let maybe_window = self.find_wayland_window(window_name);

        let is_x11 = maybe_window
            .as_ref()
            .map(|w| w.x11_surface().is_some())
            .unwrap_or(false);

        let surface = maybe_window
            .as_ref()
            .and_then(|window| {
                window
                    .toplevel()
                    .map(|t| t.wl_surface().clone())
                    .or_else(|| window.x11_surface().and_then(|x| x.wl_surface()))
            })
            .or_else(|| {
                tracing::warn!(
                    "handle_forwarded_keyboard_event: using current keyboard focus fallback for {}",
                    window_name
                );
                self.seat.get_keyboard().and_then(|kb| kb.current_focus())
            });

        let surface = match surface {
            Some(s) => s,
            None => {
                tracing::warn!(
                    "handle_forwarded_keyboard_event: no target surface (window={}, x11={})",
                    window_name,
                    is_x11
                );
                return;
            }
        };

        let keyboard = match self.seat.get_keyboard() {
            Some(kb) => kb,
            None => return,
        };
        let serial = SERIAL_COUNTER.next_serial();
        let had_focus = keyboard.current_focus().as_ref() == Some(&surface);
        if !had_focus {
            tracing::info!(
                "Setting keyboard focus to {} window: {}",
                if is_x11 { "X11" } else { "Wayland" },
                window_name
            );
            keyboard.set_focus(self, Some(surface.clone()), serial);
        }

        let xkb_code = evdev_key + 8;
        tracing::trace!(
            "Forwarding keyboard event to {}: key={} (xkb={}), pressed={}",
            if is_x11 { "X11" } else { "Wayland" },
            evdev_key,
            xkb_code,
            pressed
        );
        keyboard.input::<(), _>(
            self,
            xkb_code.into(),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            serial,
            time,
            |_state, _modifiers, _handle| FilterResult::Forward,
        );
    }

    /// Forward a pointer button event (press/release) from the web UI.
    /// Moves the pointer to (x, y) within the window first, then sends the button event.
    pub fn handle_forwarded_pointer_button(
        &mut self,
        window_name: &str,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        time: u32,
    ) {
        use smithay::input::pointer::ButtonEvent;

        // Update pointer location
        self.handle_forwarded_pointer_event(window_name, x, y);

        let pointer = match self.seat.get_pointer() {
            Some(p) => p,
            None => return,
        };

        if pressed {
            if let Some(window) = self.find_wayland_window(window_name) {
                let is_x11 = window.x11_surface().is_some();
                let surface = window
                    .toplevel()
                    .map(|t| t.wl_surface().clone())
                    .or_else(|| window.x11_surface().and_then(|x| x.wl_surface()));

                if let Some(surface) = surface {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let focus_serial = SERIAL_COUNTER.next_serial();
                        tracing::info!(
                            "Pointer button press: setting keyboard focus to {} window: {}",
                            if is_x11 { "X11" } else { "Wayland" },
                            window_name
                        );
                        keyboard.set_focus(self, Some(surface), focus_serial);
                    }
                } else {
                    tracing::warn!(
                        "Pointer button: {} window {} has no wl_surface",
                        if is_x11 { "X11" } else { "Wayland" },
                        window_name
                    );
                }
            }
        }

        let serial = SERIAL_COUNTER.next_serial();
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time,
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        );
        pointer.frame(self);
    }

    /// Forward a pointer scroll event from the web UI.
    pub fn handle_forwarded_pointer_scroll(&mut self, window_name: &str, dx: f64, dy: f64) {
        use smithay::input::pointer::AxisFrame;

        if self.find_wayland_window(window_name).is_none() {
            return;
        }

        let pointer = match self.seat.get_pointer() {
            Some(p) => p,
            None => return,
        };

        let mut frame = AxisFrame::new(0).source(AxisSource::Wheel);
        if dx != 0.0 {
            frame = frame.value(Axis::Horizontal, dx);
        }
        if dy != 0.0 {
            frame = frame.value(Axis::Vertical, dy);
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Forward relative motion received from the web UI to the Wayland surface.
    pub fn handle_forwarded_relative_pointer_event(&mut self, window_name: &str, dx: f64, dy: f64) {
        use smithay::input::pointer::RelativeMotionEvent;

        let window = match self.find_wayland_window(window_name) {
            Some(w) => w,
            None => return,
        };

        let surface = window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .or_else(|| window.x11_surface().and_then(|x| x.wl_surface().clone()));

        if let Some(surface) = surface {
            if let Some(pointer) = self.seat.get_pointer() {
                let event = RelativeMotionEvent {
                    utime: 0,
                    delta: (dx, dy).into(),
                    delta_unaccel: (dx, dy).into(),
                };
                pointer.relative_motion(self, Some((surface, (0.0, 0.0).into())), &event);
                pointer.frame(self);
            }
        }
    }
}
