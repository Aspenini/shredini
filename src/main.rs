#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(not(target_arch = "wasm32"))]
mod storage;
#[cfg(not(target_arch = "wasm32"))]
mod ui;
#[cfg(not(target_arch = "wasm32"))]
use shredini_core::{media, midi, model};

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--probe-midi") => {
            if let Err(error) = midi::probe(15) {
                eprintln!("{error:#}");
            }
        }
        Some("--list-midi") => {
            let (ports, note) = midi::enumerate();
            for port in ports {
                println!("{} | {}", port.name, port.id);
            }
            if !note.is_empty() {
                println!("{note}");
            }
        }
        Some("--list-devices") => shredini_engine::print_devices(),
        _ => ui::run(),
    }
}
