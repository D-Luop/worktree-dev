//! `wtd-tray.exe`: the notification-area icon as a GUI-subsystem program, so starting it (at logon,
//! from Settings) never opens a console window. Same code as `wtd tray`.
#![windows_subsystem = "windows"]

fn main() {
    let code = wtdlib::tray::main(&[]).unwrap_or(1);
    std::process::exit(code);
}
