//! Zwischenablage beobachten und Änderungen als Eingabe melden.

use super::*;

pub(super) fn clipboard_watcher(tx: Sender<Event>, stop: Arc<AtomicBool>) {
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Zwischenablage nicht verfügbar: {e}");
            return;
        }
    };
    let mut last = cb.get_text().ok();
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(600));
        if let Ok(t) = cb.get_text() {
            if last.as_deref() != Some(t.as_str()) {
                last = Some(t.clone());
                let _ = tx.send(Event::Input(Input::LocalClipboard(t)));
            }
        }
    }
}
