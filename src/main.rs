#![cfg_attr(not(test), windows_subsystem = "windows")]

mod install;
mod render;
mod servers;
mod theme;
mod ui;

use std::ptr::null_mut;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [arg] if arg == "--install" => return install::install(),
        [] => {}
        _ => return Err("Use --install, or run without arguments.".into()),
    }
    match install::Instance::acquire()? {
        Some(_instance) => ui::run(),
        None => Ok(()),
    }
}

fn main() {
    if let Err(error) = run() {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(&error).as_ptr(),
                wide("Portside").as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}
