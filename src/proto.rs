//! Generated bindings for the KWin-specific Wayland protocols we rely on.
//! The XML files are vendored from plasma-wayland-protocols (LGPL-2.1-or-later).

#![allow(dead_code, non_upper_case_globals, non_camel_case_types, clippy::all)]

pub mod screencast {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

pub mod fake_input {
    use wayland_client;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/fake-input.xml");
}
