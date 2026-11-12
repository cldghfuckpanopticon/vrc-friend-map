// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Before GTK/WebKit or WebView2 start: they read these at startup.
    vrc_friend_map_lib::graphics::apply_early();
    vrc_friend_map_lib::run()
}
