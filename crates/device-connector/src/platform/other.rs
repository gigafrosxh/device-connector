//! Platzhalter für andere Systeme (Linux). Damit lassen sich Kern und
//! Service auch dort bauen und testen; Ein- und Ausgabe sind nicht umgesetzt.

use dc_core::{Inject, Rect};
use std::path::PathBuf;

use super::{HandoffTarget, Sink};

pub struct InputHandle;

impl InputHandle {
    pub fn set_capturing(&self, _on: bool) {}
    pub fn warp(&self, _x: f64, _y: f64) {}
}

pub fn start_input(_sink: Sink) -> Result<InputHandle, String> {
    Err("Maus/Tastatur abgreifen ist auf diesem System nicht umgesetzt".into())
}

pub fn screen_bounds() -> Rect {
    Rect::new(0.0, 0.0, 1920.0, 1080.0)
}

pub struct Injector;

impl Injector {
    pub fn new() -> Result<Injector, String> {
        Ok(Injector)
    }
    pub fn inject(&mut self, _i: &Inject) {}
}

pub fn selected_files() -> Vec<PathBuf> {
    Vec::new()
}

pub fn handoff_target() -> Option<HandoffTarget> {
    None
}

pub fn cancel_local_drag() {}

pub fn ask_text(_title: &str, _prompt: &str, _default: &str) -> Option<String> {
    None
}

pub fn confirm(_title: &str, _text: &str, _yes: &str) -> bool {
    false
}

pub fn show_info(title: &str, text: &str) {
    eprintln!("{title}: {text}");
}

pub fn missing_permissions(_prompt: bool) -> Vec<String> {
    Vec::new()
}

pub fn open_permission_settings() {}

pub fn can_inject() -> bool {
    false
}

pub fn pick_files() -> Vec<PathBuf> {
    Vec::new()
}

pub struct FileDrag;

impl FileDrag {
    pub fn cancel(&self) {}
}

pub fn start_file_drag(
    _items: &[PathBuf],
    _x: f64,
    _y: f64,
    _on_end: Box<dyn FnOnce(bool) + Send>,
) -> Result<FileDrag, String> {
    Err("auf diesem System nicht unterstützt".into())
}

pub fn file_manager_in_front() -> bool {
    false
}
