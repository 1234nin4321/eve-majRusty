//! Client bindings for hyprland-toplevel-export-v1 (from hyprland-protocols, BSD-3-Clause; see the XML's copyright
//! block). It references wlr-foreign-toplevel-management's handle type, which wayland-protocols-wlr provides.

#![allow(dead_code, non_upper_case_globals, non_camel_case_types, clippy::all)]

pub mod hyprland_toplevel_export {
    use wayland_client;
    use wayland_client::protocol::*;
    pub use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}
