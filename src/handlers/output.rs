use smithay::{
    delegate_output,
    output::{Output, PhysicalProperties, Subpixel},
    utils::Transform,
    wayland::output::OutputHandler,
};

use crate::state::WoState;

impl OutputHandler for WoState {}
delegate_output!(WoState);

impl WoState {
    /// Create and register a single logical output with Smithay.
    pub fn create_output(
        width: i32,
        height: i32,
        refresh_mhz: i32,
        display: &smithay::reexports::wayland_server::DisplayHandle,
    ) -> Output {
        let output = Output::new(
            "wo-output".to_string(),
            PhysicalProperties {
                size:    (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make:    "Wo".into(),
                model:   "Virtual".into(),
            },
        );

        let mode = smithay::output::Mode {
            size:    (width, height).into(),
            refresh: refresh_mhz,
        };

        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(smithay::output::Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        output.create_global::<WoState>(display);

        output
    }
}
