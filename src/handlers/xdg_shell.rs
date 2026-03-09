use smithay::{
    delegate_xdg_shell,
    desktop::{PopupManager, Space, Window},
    reexports::wayland_server::{protocol::{wl_output, wl_seat}, Resource},
    utils::Serial,
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;

use crate::state::WoState;

/// Height of the comraw top panel in pixels.
pub const PANEL_H: i32 = 32;

/// Height of the comraw MacWindow title bar in pixels.
/// Only applied to windows that negotiated SSD via xdg-decoration.
/// CSD windows (GTK4/libadwaita/Firefox) draw their own decorations.
pub const TITLE_H: i32 = 30;

impl XdgShellHandler for WoState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::info!("new_toplevel ENTER");
        let app_id = with_states(surface.wl_surface(), |states| {
            states.data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|data| data.app_id.clone()))
        });
        tracing::info!(app_id = ?app_id, "new_toplevel: native Wayland window created");
        self.debug_new_toplevel = true;
        self.metadata_dirty = true;

        let oid = surface.wl_surface().id();
        let stable_name = format!("wayland-{}", self.next_wayland_id);
        self.next_wayland_id += 1;
        self.wayland_window_names.insert(oid.clone(), stable_name.clone());
        self.wayland_name_to_id.insert(stable_name.clone(), oid.clone());
        tracing::info!(name = %stable_name, "assigned stable window name");
        // Mark this Wayland window as mapped/visible for metadata and frontend
        self.window_mapped.insert(stable_name.clone(), true);

        self.ssd_windows.remove(&oid);

        let window = Window::new(surface);
        self.all_windows.push(window.clone());

        let parent_surface = window.toplevel().and_then(|t| {
            with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok().and_then(|data| data.parent.clone()))
            })
        });


        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);

        let location = if let Some(ref parent_wl) = parent_surface {
            let parent_center = self
                .space
                .elements()
                .find(|w| {
                    w.toplevel()
                        .map(|t| t.wl_surface() == parent_wl)
                        .unwrap_or(false)
                })
                .map(|parent_win| {
                    let loc = self.space.element_location(parent_win).unwrap_or_default();
                    let geo = parent_win.geometry();
                    (loc.x + geo.size.w / 2, loc.y + geo.size.h / 2)
                })
                .unwrap_or_else(|| (ow / 2, oh / 2));
            let dialog_y = (parent_center.1 - 75).max(PANEL_H);
            (parent_center.0 - 200, dialog_y)
        } else {
            let suggest_w = (ow * 2 / 3).max(640).min(1600);
            let suggest_h = (oh * 2 / 3).max(480).min(1000);
            let center_y = ((oh - suggest_h) / 2).max(PANEL_H + 20);
            ((ow - suggest_w) / 2, center_y)
        };

        self.space.map_element(window.clone(), location, true);
        // (clients that use xdg_decoration will get a sized configure later).
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_configure();
            
            // Give keyboard focus to newly mapped windows (unless they're dialogs with parents)
            // This ensures games and apps receive keyboard input immediately
            if parent_surface.is_none() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                }
            }
        }
        tracing::info!("new_toplevel EXIT (mapped + configured)");
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let mut geo = positioner.get_geometry();

        // Constrain the popup to stay within the output bounds.
        // We look up the parent window's screen position so we can compute the
        // absolute popup rectangle and clamp it back into [0, output_size].
        let parent_loc: Option<smithay::utils::Point<i32, smithay::utils::Logical>> =
            smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
                states.data_map
                    .get::<smithay::wayland::shell::xdg::XdgPopupSurfaceData>()
                    .and_then(|d| d.lock().ok().and_then(|locked| locked.parent.clone()))
            })
            .and_then(|parent_wl| {
                self.space
                    .elements()
                    .find(|w| {
                        w.toplevel()
                            .map(|t| t.wl_surface() == &parent_wl)
                            .unwrap_or(false)
                    })
                    .and_then(|w| self.space.element_location(w))
            });

        if let Some(parent_loc) = parent_loc {
            let abs_x = parent_loc.x + geo.loc.x;
            let abs_y = parent_loc.y + geo.loc.y;
            let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);

            let clamped_abs_x = abs_x.clamp(0, (ow - geo.size.w).max(0));
            let clamped_abs_y = abs_y.clamp(0, (oh - geo.size.h).max(0));

            geo.loc.x = clamped_abs_x - parent_loc.x;
            geo.loc.y = clamped_abs_y - parent_loc.y;
        }

        surface.with_pending_state(|s| {
            s.geometry = geo;
        });
        // XDG shell protocol requires the compositor to send an initial configure
        // immediately after xdg_popup is created, before any surface commit.
        // Without this, clients that wait for configure before committing will hang.
        if let Err(e) = surface.send_configure() {
            tracing::warn!("failed to send initial popup configure: {e}");
        }
        if let Err(e) = self.popup_manager.track_popup(smithay::desktop::PopupKind::Xdg(surface)) {
            tracing::warn!("failed tracking popup: {e}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Find the window element for this surface and start tracking a move.
        let window = self.space.elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            let initial_loc = self.space.element_location(&window).unwrap_or_default();
            self.grab_state = Some(GrabState::Move {
                window,
                initial_window_location: initial_loc,
                initial_pointer: self.pointer_location,
            });
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat:    wl_seat::WlSeat,
        _serial:  Serial,
        edges:    ResizeEdge,
    ) {
        let window = self.space.elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            let initial_loc = self.space.element_location(&window).unwrap_or_default();
            let geo = window.geometry();
            self.grab_state = Some(GrabState::Resize {
                surface,
                window,
                edges,
                initial_window_location: initial_loc,
                initial_window_size: (geo.size.w, geo.size.h),
                initial_pointer: self.pointer_location,
            });
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) { }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        let window = self.space.elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;

            let th = self.title_h_for_window(&window);
            let y_offset = th.max(PANEL_H);
            let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
            let content_h = (oh - y_offset).max(1);

            surface.with_pending_state(|state| {
                state.states.set(XdgState::Fullscreen);
                state.states.unset(XdgState::Maximized);
                state.size = Some((ow, content_h).into());
            });
            surface.send_configure();
            self.space.map_element(window.clone(), (0, y_offset), false);
            self.metadata_dirty = true;
            
            // Automatically give keyboard focus to fullscreen windows (important for games)
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let window = self.space.elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;

            let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
            let suggest_w = (ow * 2 / 3).max(640).min(1600);
            let suggest_h = (oh * 2 / 3).max(480).min(1000);

            surface.with_pending_state(|state| {
                state.states.unset(XdgState::Fullscreen);
                if state.size.is_none() {
                    state.size = Some((suggest_w, suggest_h).into());
                }
            });
            surface.send_configure();
            self.metadata_dirty = true;
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        tracing::info!("toplevel_destroyed called");
        self.metadata_dirty = true;

        // Clean up stable window name mappings.
        let oid = surface.wl_surface().id();
        if let Some(name) = self.wayland_window_names.remove(&oid) {
            self.wayland_name_to_id.remove(&name);
            if self.keyboard_window_focus.as_deref() == Some(&name) {
                self.keyboard_window_focus = None;
            }
            if self.pointer_window_focus.as_deref() == Some(&name) {
                self.pointer_window_focus = None;
            }
            tracing::info!(name = %name, "removed stable window name");
        }
        self.ssd_windows.remove(&oid);

        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus().as_ref() == Some(surface.wl_surface()) {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, None, serial);
            }
        }

        let to_remove = self
            .all_windows
            .iter()
            .find(|w| {
                w.toplevel()
                    .map(|t| t == &surface)
                    .unwrap_or(false)
            })
            .cloned();

        if let Some(window) = to_remove {
            self.all_windows.retain(|w| w != &window);
            self.space.unmap_elem(&window);
            self.refocus_topmost_native_window();
        }
    }
}

delegate_xdg_shell!(WoState);

/// Active grab state for interactive move/resize of Wayland client windows.
#[derive(Debug, Clone)]
pub enum GrabState {
    Move {
        window: Window,
        initial_window_location: smithay::utils::Point<i32, smithay::utils::Logical>,
        initial_pointer: smithay::utils::Point<f64, smithay::utils::Logical>,
    },
    Resize {
        surface: ToplevelSurface,
        window: Window,
        edges: ResizeEdge,
        initial_window_location: smithay::utils::Point<i32, smithay::utils::Logical>,
        initial_window_size: (i32, i32),
        initial_pointer: smithay::utils::Point<f64, smithay::utils::Logical>,
    },
}

/// Send the initial configure to a surface if it hasn't received one yet.
pub fn ensure_initial_configure(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    space:   &Space<Window>,
    popups:  &mut PopupManager,
) {

    if let Some(window) = space
        .elements()
        .find(|w| {
            w.toplevel()
                .map(|t| t.wl_surface() == surface)
                .unwrap_or(false)
        })
        .cloned()
    {
        if let Some(toplevel) = window.toplevel() {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .map(|d| d.lock().unwrap().initial_configure_sent)
                    .unwrap_or(false)
            });

            if !initial_configure_sent {
                toplevel.send_configure();
            }
        }
        return;
    }

    // Handle popups.
    if let Some(popup) = popups.find_popup(surface) {
        if let smithay::desktop::PopupKind::Xdg(ref xdg) = popup {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgPopupSurfaceData>()
                    .map(|d| d.lock().unwrap().initial_configure_sent)
                    .unwrap_or(false)
            });
            if !initial_configure_sent {
                xdg.send_configure().ok();
            }
        }
    }
}
