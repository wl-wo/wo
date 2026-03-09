//! XWayland window manager handler implementation for the Wo compositor.
//!
//! Implements [`XwmHandler`] and [`XWaylandShellHandler`] so that X11 clients
//! running under XWayland can be managed by the compositor.

use smithay::{
    desktop::Window,
    utils::{Logical, Rectangle},
    wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    xwayland::xwm::{
        Reorder, ResizeEdge as X11ResizeEdge, X11Surface, X11Wm, XwmHandler, XwmId,
    },
};
use smithay::xwayland::xwm::X11Window;
use smithay::reexports::wayland_server::Resource;
use tracing::{info, warn};

use crate::state::WoState;
use crate::handlers::xdg_shell::PANEL_H;

impl XwmHandler for WoState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("xwm not initialized")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: new X11 window created"
        );
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: new override-redirect window"
        );
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            title = %window.title(),
            class = %window.class(),
            "XWayland: map window request"
        );

        if let Err(e) = window.set_mapped(true) {
            warn!("XWayland: failed to set_mapped(true): {e}");
            return;
        }

        let stable_name = format!("x11-{}", window.window_id());
        self.x11_window_names.insert(window.window_id(), stable_name.clone());

        let parent_x11_id = window.is_transient_for();
        let geo = window.geometry();
        
        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
        
        let location = if let Some(parent_id) = parent_x11_id {
            info!(
                window_id = window.window_id(),
                parent_id = parent_id,
                "XWayland: window is transient-for (dialog)"
            );
            
            let parent_center = self
                .space
                .elements()
                .find(|w| {
                    w.x11_surface()
                        .map(|x| x.window_id() == parent_id)
                        .unwrap_or(false)
                })
                .and_then(|parent_win| {
                    let loc = self.space.element_location(parent_win)?;
                    let geo = parent_win.geometry();
                    Some((loc.x + geo.size.w / 2, loc.y + geo.size.h / 2))
                })
                .unwrap_or_else(|| (ow / 2, oh / 2));
            
            let dialog_y = (parent_center.1 - 75).max(PANEL_H);
            (parent_center.0 - 200, dialog_y)
        } else {
            let adjusted_y = geo.loc.y.max(PANEL_H);
            (geo.loc.x, adjusted_y)
        };
        
        let win = Window::new_x11_window(window.clone());
        self.all_windows.push(win.clone());
        self.space.map_element(win, location, true);
        self.metadata_dirty = true;
        
        if parent_x11_id.is_none() {
            if let Some(wl_surface) = window.wl_surface() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, Some(wl_surface), serial);
                }
            }

            if let Some(name) = self.x11_window_names.get(&window.window_id()).cloned() {
                self.keyboard_window_focus = Some(name.clone());
                self.pointer_window_focus = Some(name);
            }
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: override-redirect window mapped"
        );

        let geo = window.geometry();
        let adjusted_y = geo.loc.y.max(PANEL_H);
        let win = Window::new_x11_window(window);
        self.all_windows.push(win.clone());
        self.space.map_element(win, (geo.loc.x, adjusted_y), false);
        self.metadata_dirty = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: window unmapped"
        );

        if let Some(name) = self.x11_window_names.get(&window.window_id()).cloned() {
            if self.keyboard_window_focus.as_deref() == Some(&name) {
                self.keyboard_window_focus = None;
            }
            if self.pointer_window_focus.as_deref() == Some(&name) {
                self.pointer_window_focus = None;
            }
        }

        if let Some(wl_surface) = window.wl_surface() {
            if let Some(keyboard) = self.seat.get_keyboard() {
                if keyboard.current_focus().as_ref() == Some(&wl_surface) {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, None, serial);
                }
            }
        }

        let to_remove = self.space.elements().find(|w| {
            w.x11_surface()
                .map(|x| x.window_id() == window.window_id())
                .unwrap_or(false)
        }).cloned();

        if let Some(w) = to_remove {
            self.space.unmap_elem(&w);
            self.all_windows.retain(|win| win != &w);
            self.refocus_topmost_native_window();
        }
        self.metadata_dirty = true;
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: window destroyed"
        );

        if let Some(name) = self.x11_window_names.get(&window.window_id()).cloned() {
            if self.keyboard_window_focus.as_deref() == Some(&name) {
                self.keyboard_window_focus = None;
            }
            if self.pointer_window_focus.as_deref() == Some(&name) {
                self.pointer_window_focus = None;
            }
        }

        if let Some(wl_surface) = window.wl_surface() {
            if let Some(keyboard) = self.seat.get_keyboard() {
                if keyboard.current_focus().as_ref() == Some(&wl_surface) {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, None, serial);
                }
            }
        }

        self.x11_window_names.remove(&window.window_id());

        let to_remove = self.space.elements().find(|w| {
            w.x11_surface()
                .map(|x| x.window_id() == window.window_id())
                .unwrap_or(false)
        }).cloned();
        if let Some(w) = to_remove {
            self.space.unmap_elem(&w);
            self.all_windows.retain(|win| win != &w);
            self.refocus_topmost_native_window();
        }
        self.metadata_dirty = true;
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        _w: Option<u32>,
        _h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        if let Err(e) = window.configure(None) {
            warn!("XWayland: configure failed: {e}");
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<X11Window>,
    ) {}

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: X11ResizeEdge,
    ) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: fullscreen request"
        );

        if let Err(e) = window.set_fullscreen(true) {
            warn!("XWayland: failed to set fullscreen: {e}");
            return;
        }

        let th = if let Some(wl_surface) = window.wl_surface() {
            if self.ssd_windows.contains(&wl_surface.id()) {
                30
            } else {
                0
            }
        } else {
            0
        };
        let y_offset = th.max(PANEL_H);
        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
        let content_h = (oh - y_offset).max(1);

        if let Err(e) = window.configure(Some(Rectangle::from_loc_and_size((0, y_offset), (ow, content_h)))) {
            warn!("XWayland: fullscreen configure failed: {e}");
        }

        let win = self.space.elements().find(|w| {
            w.x11_surface()
                .map(|x| x.window_id() == window.window_id())
                .unwrap_or(false)
        }).cloned();

        if let Some(w) = win {
            self.space.map_element(w, (0, y_offset), false);
        }

        // Automatically give keyboard focus to fullscreen windows (important for games)
        if let Some(wl_surface) = window.wl_surface() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(wl_surface.clone()), serial);
            }
        }

        if let Some(name) = self.x11_window_names.get(&window.window_id()).cloned() {
            self.keyboard_window_focus = Some(name.clone());
            self.pointer_window_focus = Some(name);
        }

        self.metadata_dirty = true;
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            window_id = window.window_id(),
            "XWayland: unfullscreen request"
        );

        if let Err(e) = window.set_fullscreen(false) {
            warn!("XWayland: failed to unset fullscreen: {e}");
            return;
        }

        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
        let suggest_w = (ow * 2 / 3).max(640).min(1600);
        let suggest_h = (oh * 2 / 3).max(480).min(1000);

        if let Err(e) = window.configure(Some(Rectangle::from_loc_and_size((0, 0), (suggest_w, suggest_h)))) {
            warn!("XWayland: unfullscreen configure failed: {e}");
        }
        self.metadata_dirty = true;
    }
}

impl XWaylandShellHandler for WoState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.xwayland_shell_state
            .as_mut()
            .expect("xwayland_shell_state not initialized")
    }

    fn surface_associated(
        &mut self,
        _xwm: XwmId,
        wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        window: X11Surface,
    ) {
        info!(
            window_id = window.window_id(),
            wl_surface = ?wl_surface.id(),
            "XWayland: X11 window associated with wl_surface"
        );
    }
}

smithay::delegate_xwayland_shell!(WoState);
