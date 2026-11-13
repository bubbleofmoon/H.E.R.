//! Shared fixture world for the integration targets: held loopback ports, owned console startup,
//! one fake OneBot gateway, disposable runtime roots, the dependency map and hidden child windows.
//! A target includes it with `mod support;`. Never add a `tests/*.rs` file for it (each one is a CI target).
#![allow(dead_code)]

pub mod gateway;
#[path = "../../src/install_root.rs"]
pub mod install_root;
pub mod ports;
pub mod runtime;

#[allow(unused_imports)]
pub use gateway::FakeGateway;
#[allow(unused_imports)]
pub use runtime::{write_dependencies, RuntimeRoot, CLOSED};

use std::process::Command;

/// Starts children without a console window on Windows; a no-op elsewhere.
pub fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}
