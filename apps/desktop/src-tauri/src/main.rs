// SPDX-License-Identifier: MIT
// Prevents an additional console window on Windows in release. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    macalla_desktop_lib::run()
}
