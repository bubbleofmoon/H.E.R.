#![cfg_attr(windows, windows_subsystem = "windows")]

#[macro_use]
mod output;
mod install_root;
include!("entry.rs");
