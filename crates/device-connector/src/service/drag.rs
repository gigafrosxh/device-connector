//! Dateien über den Bildschirmrand ziehen: abgebende und empfangende Seite,
//! verdeckte Bereitstellungsordner.

use super::*;

/// Abgebende Seite eines Datei-Drags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum OutDrag {
    None,
    /// Markierte Dateien werden noch abgefragt. `crossed`: Rand schon
    /// überquert (sonst Vorab-Übertragung, #3).
    Resolving {
        crossed: bool,
        released: bool,
        cancelled: bool,
    },
    /// DragBegin und Dateien sind unterwegs.
    Sent {
        transfer: u32,
        crossed: bool,
    },
}

/// Empfangende Seite eines Datei-Drags.
pub(super) struct InDrag {
    /// Übertragung, die zu diesem Ziehen gehört.
    pub(super) transfer: u32,
    /// Bereitstellungsordner dieses Ziehens (eigener pro Ziehen).
    pub(super) dir: PathBuf,
    /// Alle Dateien sind angekommen.
    pub(super) ready: bool,
    pub(super) released: bool,
    pub(super) cancelled: bool,
    /// Oberste bereitgestellte Einträge (sobald alle Dateien da sind).
    pub(super) items: Vec<PathBuf>,
    /// Das Ziehen am Zeiger läuft.
    pub(super) active: Option<platform::FileDrag>,
    /// Gegenrichtung: Ziehen begann drüben, während wir den anderen Rechner
    /// steuerten, und kam mit gehaltener Taste hierher zurück (#3).
    pub(super) reverse: bool,
}

/// Elternordner für die verdeckten Bereitstellungsordner gezogener Dateien.
pub(super) fn staging_root() -> PathBuf {
    std::env::temp_dir().join("DeviceConnector-drag")
}

/// Markierung, an der wir eigene Bereitstellungsordner erkennen.
pub(super) const STAGING_MARKER: &str = ".device-connector-staging";

/// Neuen, privaten Bereitstellungsordner für ein Ziehen anlegen (#17).
pub(super) fn create_staging_dir(transfer: u32) -> std::io::Result<PathBuf> {
    create_staging_dir_in(&staging_root(), transfer)
}

fn create_staging_dir_in(root: &Path, transfer: u32) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    if std::fs::symlink_metadata(root)?.is_symlink() {
        return Err(std::io::Error::other(
            "Bereitstellungsordner ist eine Verknüpfung",
        ));
    }
    let dir = root.join(format!(
        "{}-{transfer}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    }
    #[cfg(not(unix))]
    std::fs::DirBuilder::new().create(&dir)?;
    std::fs::write(dir.join(STAGING_MARKER), b"")?;
    Ok(dir)
}

/// Eigene, liegen gebliebene Bereitstellungsordner älter als `age` löschen.
/// Fremde Ordner (ohne Markierung) und Verknüpfungen werden nie angefasst.
pub(super) fn cleanup_staging(age: Duration) {
    cleanup_staging_in(&staging_root(), age)
}

fn cleanup_staging_in(root: &Path, age: Duration) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if !meta.is_dir() || !p.join(STAGING_MARKER).is_file() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|el| el >= age);
        if old {
            log::debug!("Räume alten Bereitstellungsordner auf: {}", p.display());
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Bereitstellungsordner entfernen, wenn nur noch die Markierung drin ist.
pub(super) fn remove_staging_if_empty(dir: &Path) {
    let only_marker = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().all(|e| e.file_name() == STAGING_MARKER))
        .unwrap_or(false);
    if only_marker {
        let _ = std::fs::remove_dir_all(dir);
    }
}

impl Service {
    /// Alle Dateien eines Drags sind im verdeckten Ordner.
    pub(super) fn on_drag_files_ready(
        &mut self,
        files: Vec<PathBuf>,
        dirs: Vec<PathBuf>,
        base: &Path,
    ) {
        let Some(d) = self.in_drag.as_mut() else {
            return;
        };
        let mut all = files.clone();
        all.extend(dirs);
        d.items = files::top_level(&all, base);
        d.ready = true;
        log::info!("{} Datei(en) für das Ziehen bereit", files.len());
        self.try_start_incoming_drag();
    }

    /// Bereitgestellte Dateien an den Zeiger hängen, sobald sie da sind und
    /// der Zeiger hier ist. Vorher (Vorab-Übertragung) wird gewartet.
    pub(super) fn try_start_incoming_drag(&mut self) {
        let Some(d) = self.in_drag.as_ref() else {
            return;
        };
        if !d.ready || d.active.is_some() {
            return;
        }
        let mut d = self.in_drag.take().unwrap();
        if d.items.is_empty() {
            let _ = std::fs::remove_dir_all(&d.dir);
            return;
        }
        if d.released || d.cancelled {
            let reveal = d.released;
            self.store_drag_items(&d.items, reveal, "Dateien liegen im Empfangsordner");
            remove_staging_if_empty(&d.dir);
            return;
        }
        let here = self.engine.mode() == Mode::Controlled;
        let back = d.reverse && self.engine.mode() == Mode::Local;
        if back && !self.engine.local_left_held() {
            // Schon losgelassen, bevor die Dateien da waren.
            self.store_drag_items(&d.items, true, "Dateien liegen im Empfangsordner");
            remove_staging_if_empty(&d.dir);
            return;
        }
        if !here && !back {
            // Zeiger noch nicht hier: warten (DragCancel/DragDiscard räumen auf).
            self.in_drag = Some(d);
            return;
        }
        let (x, y) = if back {
            platform::cursor_pos().unwrap_or_else(|| self.engine.cursor())
        } else {
            self.engine.cursor()
        };
        let tx = self.tx.clone();
        let items = d.items.clone();
        let on_end = Box::new(move |dropped: bool| {
            let _ = tx.send(Event::DragEnded { dropped, items });
        });
        match platform::start_file_drag(&d.items, x, y, on_end) {
            Ok(h) => {
                d.active = Some(h);
                self.in_drag = Some(d);
                if back {
                    // Das System hier hat das Drücken nicht gesehen (es ging
                    // an den anderen Rechner): nachholen. Das echte Loslassen
                    // kommt dann ganz normal an und legt die Dateien ab.
                    if let Some(inj) = &mut self.injector {
                        inj.inject(&dc_core::Inject::Button {
                            button: dc_core::Button::Left,
                            down: true,
                            x,
                            y,
                        });
                    }
                } else {
                    let acts = self.engine.inject_left(true);
                    for a in acts {
                        self.execute(a);
                    }
                }
                self.set_note(Some(
                    "Dateien hängen am Zeiger: loslassen zum Ablegen".into(),
                ));
            }
            Err(e) => {
                log::warn!("Ziehen am Zeiger nicht möglich: {e}");
                self.store_drag_items(&d.items, true, "Dateien liegen im Empfangsordner");
                remove_staging_if_empty(&d.dir);
            }
        }
    }

    /// Laufendes Ziehen abbrechen und die Dateien in den Empfangsordner legen.
    pub(super) fn cancel_incoming_drag(&mut self, why: &str) {
        let Some(mut d) = self.in_drag.take() else {
            return;
        };
        if let Some(h) = d.active.take() {
            h.cancel();
        }
        log::info!("{why}");
        let items = std::mem::take(&mut d.items);
        self.store_drag_items(
            &items,
            false,
            &format!("{why}: Dateien liegen im Empfangsordner"),
        );
        remove_staging_if_empty(&d.dir);
    }

    pub(super) fn store_drag_items(&mut self, items: &[PathBuf], reveal: bool, note: &str) {
        let dir = self.cfg.receive_dir();
        let mut first = None;
        for it in items {
            match files::move_into(it, &dir) {
                Ok(p) => {
                    first.get_or_insert(p);
                }
                Err(e) => log::warn!("{} verschieben: {e}", it.display()),
            }
        }
        log::info!(
            "{} Eintrag/Einträge nach {} gelegt",
            items.len(),
            dir.display()
        );
        if reveal {
            if let Some(f) = &first {
                let _ = opener::reveal(f);
            }
        }
        self.set_note(Some(note.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dc-staging-test-{name}-{}-{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn staging_dirs_are_private_and_marked() {
        let root = temp_root("create");
        let a = create_staging_dir_in(&root, 7).unwrap();
        let b = create_staging_dir_in(&root, 7).unwrap();
        assert_ne!(a, b, "jedes Ziehen bekommt einen eigenen Ordner");
        assert!(a.starts_with(&root));
        assert!(a.join(STAGING_MARKER).is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_refused() {
        let root = temp_root("link");
        let real = temp_root("link-target");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &root).unwrap();
        assert!(create_staging_dir_in(&root, 1).is_err());
        let _ = std::fs::remove_file(&root);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn cleanup_only_touches_own_old_dirs() {
        let root = temp_root("cleanup");
        let own = create_staging_dir_in(&root, 1).unwrap();
        std::fs::write(own.join("datei.txt"), b"x").unwrap();
        let foreign = root.join("fremd");
        std::fs::create_dir_all(&foreign).unwrap();
        // Noch nicht alt genug: bleibt.
        cleanup_staging_in(&root, Duration::from_secs(3600));
        assert!(own.exists());
        cleanup_staging_in(&root, Duration::ZERO);
        assert!(!own.exists(), "eigener Ordner wird aufgeräumt");
        assert!(foreign.exists(), "fremde Ordner bleiben");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staging_dir_is_removed_only_when_empty() {
        let root = temp_root("empty");
        let d = create_staging_dir_in(&root, 2).unwrap();
        std::fs::write(d.join("datei.txt"), b"x").unwrap();
        remove_staging_if_empty(&d);
        assert!(d.exists(), "Inhalt noch da");
        std::fs::remove_file(d.join("datei.txt")).unwrap();
        remove_staging_if_empty(&d);
        assert!(!d.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
