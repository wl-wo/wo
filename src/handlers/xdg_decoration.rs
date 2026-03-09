use smithay::{
    delegate_xdg_decoration,
    reexports::wayland_server::Resource,
    wayland::shell::xdg::{
        ToplevelSurface,
        decoration::XdgDecorationHandler,
    },
};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

use crate::state::WoState;

impl XdgDecorationHandler for WoState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        tracing::info!("new_decoration: client attached xdg_decoration");
        
        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
        let suggest_w = (ow * 2 / 3).max(640).min(1600);
        let suggest_h = (oh * 2 / 3).max(480).min(1000);
        
        toplevel.with_pending_state(|state| {
            state.size = Some((suggest_w, suggest_h).into());
        });
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: Mode) {
        tracing::info!("request_mode: client requests {:?}", mode);
        
        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
        let suggest_w = (ow * 2 / 3).max(640).min(1600);
        let suggest_h = (oh * 2 / 3).max(480).min(1000);
        
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
            if state.size.is_none() {
                state.size = Some((suggest_w, suggest_h).into());
            }
        });
        match mode {
            Mode::ServerSide => {
                self.ssd_windows.insert(toplevel.wl_surface().id());
            }
            _ => {
                self.ssd_windows.remove(&toplevel.wl_surface().id());
            }
        }
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        tracing::info!("unset_mode: reverting to default client-side decorations");
        
        let (ow, oh) = (self.output_size.0 as i32, self.output_size.1 as i32);
        let suggest_w = (ow * 2 / 3).max(640).min(1600);
        let suggest_h = (oh * 2 / 3).max(480).min(1000);
        
        toplevel.with_pending_state(|state| {
            state.decoration_mode = None;
            if state.size.is_none() {
                state.size = Some((suggest_w, suggest_h).into());
            }
        });
        self.ssd_windows.remove(&toplevel.wl_surface().id());
        toplevel.send_configure();
    }
}

delegate_xdg_decoration!(WoState);
