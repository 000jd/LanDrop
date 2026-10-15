//! Desktop entry point for the LanDrop app.
//!
//! All the app logic lives in the library target (`lib.rs`) because Android
//! and iOS load the app as a cdylib/staticlib. `cargo run`, the installed
//! desktop app and `cargo tauri build` all go through here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    landrop_desktop_lib::run();
}
