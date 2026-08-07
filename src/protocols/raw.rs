// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

/// `xdg-session-management-v1`, vendored from wayland-protocols staging.
///
/// `resources/xdg-session-management-v1.xml` is a byte-for-byte copy of the file in
/// wayland-protocols 0.32.13; that release ships the XML but no generated bindings module yet, so
/// we run the scanner ourselves. Drop this once the crate exports `xdg::session_management`.
///
/// Unlike the other raw protocols, this one references `xdg_toplevel`, so the generated code needs
/// xdg-shell's interfaces in scope alongside the core ones.
pub mod xdg_session_management {
    pub mod v1 {
        #[cfg(test)]
        pub use self::generated::client;
        pub use self::generated::server;

        mod generated {
            pub mod server {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
                use smithay::reexports::wayland_server;
                use wayland_server::protocol::*;

                pub mod __interfaces {
                    use smithay::reexports::wayland_protocols::xdg::shell::server::__interfaces::*;
                    use smithay::reexports::wayland_server;
                    use wayland_server::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!(
                        "resources/xdg-session-management-v1.xml"
                    );
                }
                use self::__interfaces::*;

                wayland_scanner::generate_server_code!("resources/xdg-session-management-v1.xml");
            }

            /// The client half, for the test fixture's Wayland client only.
            #[cfg(test)]
            pub mod client {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel;
                use wayland_client;
                use wayland_client::protocol::*;

                pub mod __interfaces {
                    use smithay::reexports::wayland_protocols::xdg::shell::client::__interfaces::*;
                    use wayland_client::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!(
                        "resources/xdg-session-management-v1.xml"
                    );
                }
                use self::__interfaces::*;

                wayland_scanner::generate_client_code!("resources/xdg-session-management-v1.xml");
            }
        }
    }
}

pub mod mutter_x11_interop {
    pub mod v1 {
        pub use self::generated::server;

        mod generated {
            pub mod server {
                #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
                #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
                #![allow(missing_docs, clippy::all)]

                use smithay::reexports::wayland_server;
                use wayland_server::protocol::*;

                pub mod __interfaces {
                    use smithay::reexports::wayland_server;
                    use wayland_server::protocol::__interfaces::*;
                    wayland_scanner::generate_interfaces!("resources/mutter-x11-interop.xml");
                }
                use self::__interfaces::*;

                wayland_scanner::generate_server_code!("resources/mutter-x11-interop.xml");
            }
        }
    }
}
