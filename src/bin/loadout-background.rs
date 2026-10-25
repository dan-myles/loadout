//! The Windows Scheduled Task's program: the background update's supervisor, built for the GUI
//! subsystem so no console window opens at each run. It starts `loadout.exe` from its own
//! folder as the worker. On other systems it does nothing.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    ExitCode::from(loadout::sync::background::launch(&args))
}
