// Hides the console window that would otherwise appear alongside a release build on
// Windows. Harmless on macOS, and cheap insurance if this is ever built elsewhere.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    hearsay_lib::run()
}
