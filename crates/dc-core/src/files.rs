//! Dateiübertragung (Phase 2).
//!
//! Sender: Dateien und Ordner rekursiv einsammeln, in Blöcken über die
//! niedrige Priorität der [`Outbox`] schicken.
//! Empfänger: Pfade prüfen (kein `..`, keine absoluten Pfade), erst als
//! `.part` schreiben, am Ende umbenennen.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::link::Outbox;
use crate::protocol::Message;

pub const CHUNK: usize = 16 * 1024;
const MAX_FILES: usize = 10_000;

/// Eine zu sendende Datei mit relativem Zielpfad (`/`-getrennt).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub source: PathBuf,
    pub rel: String,
    pub size: u64,
}

pub fn collect(paths: &[PathBuf]) -> io::Result<Vec<Item>> {
    let mut out = Vec::new();
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Datei".into());
        walk(p, &name, &mut out)?;
    }
    Ok(out)
}

fn walk(path: &Path, rel: &str, out: &mut Vec<Item>) -> io::Result<()> {
    if out.len() >= MAX_FILES {
        return Ok(());
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Ok(()); // Symlinks bewusst nicht folgen
    }
    if meta.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            if name == ".DS_Store" || name == "Thumbs.db" || name == "desktop.ini" {
                continue;
            }
            walk(&e.path(), &format!("{rel}/{name}"), out)?;
        }
    } else if meta.is_file() {
        out.push(Item {
            source: path.to_path_buf(),
            rel: rel.to_string(),
            size: meta.len(),
        });
    }
    Ok(())
}

/// Sendet alle Dateien; blockiert, bis alles in der Warteschlange ist.
/// Fortschritt: `progress(gesendete_bytes, gesamt_bytes)`.
pub fn send(
    outbox: &Outbox,
    items: &[Item],
    first_id: u32,
    open_first: bool,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<()> {
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut done = 0u64;
    let closed = || io::Error::new(io::ErrorKind::BrokenPipe, "Verbindung getrennt");
    for (n, item) in items.iter().enumerate() {
        let id = first_id.wrapping_add(n as u32);
        let mut f = File::open(&item.source)?;
        if !outbox.push_bulk(Message::FileStart {
            id,
            path: item.rel.clone(),
            size: item.size,
        }) {
            return Err(closed());
        }
        let mut buf = vec![0u8; CHUNK];
        loop {
            let k = f.read(&mut buf)?;
            if k == 0 {
                break;
            }
            if !outbox.push_bulk(Message::FileChunk {
                id,
                data: buf[..k].to_vec(),
            }) {
                return Err(closed());
            }
            done += k as u64;
            progress(done, total);
        }
        if !outbox.push_bulk(Message::FileEnd { id }) {
            return Err(closed());
        }
    }
    if !outbox.push_bulk(Message::BatchEnd {
        files: items.len() as u32,
        open_first,
    }) {
        return Err(closed());
    }
    Ok(())
}

/// Wandelt einen empfangenen Pfad in einen sicheren relativen Pfad um.
pub fn sanitize(rel: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        if part.contains(['\\', ':', '\0']) {
            return None;
        }
        // Windows-reservierte Namen vermeiden.
        let upper = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "LPT1", "LPT2"].contains(&upper.as_str()) {
            return None;
        }
        let p = Path::new(part);
        if p.components().count() != 1
            || !matches!(p.components().next(), Some(Component::Normal(_)))
        {
            return None;
        }
        out.push(part.trim_end_matches([' ', '.']));
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// „Bericht.pdf“ → „Bericht (2).pdf“, wenn es schon existiert.
/// Freien Namen finden: „Bericht.pdf“ → „Bericht (2).pdf“.
pub fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    for n in 2.. {
        let candidate = parent.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Oberste Einträge eines empfangenen Vorgangs (Ordner statt aller Dateien darin).
pub fn top_level(files: &[PathBuf], base: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for f in files {
        let top = match f
            .strip_prefix(base)
            .ok()
            .and_then(|r| r.components().next())
        {
            Some(c) => base.join(c),
            None => f.clone(),
        };
        if !out.contains(&top) {
            out.push(top);
        }
    }
    out
}

/// Datei oder Ordner nach `dir` verschieben (eindeutiger Name). Klappt das
/// Umbenennen nicht (anderes Laufwerk), wird kopiert und das Original gelöscht.
pub fn move_into(src: &Path, dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "kein Dateiname"))?;
    let dest = unique(dir.join(name));
    if fs::rename(src, &dest).is_ok() {
        return Ok(dest);
    }
    copy_all(src, &dest)?;
    if src.is_dir() {
        fs::remove_dir_all(src)?;
    } else {
        fs::remove_file(src)?;
    }
    Ok(dest)
}

fn copy_all(src: &Path, dest: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dest)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_all(&e.path(), &dest.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(src, dest).map(|_| ())
    }
}

#[derive(Debug, PartialEq)]
pub enum ReceiveEvent {
    Progress {
        received: u64,
    },
    /// Vorgang abgeschlossen: alle fertigen Dateien (erste zuerst).
    Done {
        files: Vec<PathBuf>,
        open_first: bool,
    },
}

struct Incoming {
    file: File,
    tmp: PathBuf,
    target: PathBuf,
    expected: u64,
    written: u64,
}

pub struct Receiver {
    base: PathBuf,
    open: HashMap<u32, Incoming>,
    /// Abbildung oberster Name → tatsächlicher (eindeutiger) Name in diesem Vorgang.
    roots: HashMap<String, PathBuf>,
    finished: Vec<PathBuf>,
    received: u64,
}

impl Receiver {
    pub fn new(base: PathBuf) -> Receiver {
        Receiver {
            base,
            open: HashMap::new(),
            roots: HashMap::new(),
            finished: Vec::new(),
            received: 0,
        }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Zielordner für den nächsten Vorgang (z. B. verdeckter Ordner beim Ziehen).
    pub fn set_base(&mut self, base: PathBuf) {
        self.base = base;
    }

    fn target_for(&mut self, rel: &Path) -> PathBuf {
        let mut comps = rel.components();
        let root = comps
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rest: PathBuf = comps.collect();
        let base = self.base.clone();
        let root_path = self
            .roots
            .entry(root.clone())
            .or_insert_with(|| unique(base.join(&root)))
            .clone();
        if rest.as_os_str().is_empty() {
            root_path
        } else {
            root_path.join(rest)
        }
    }

    pub fn handle(&mut self, m: &Message) -> io::Result<Option<ReceiveEvent>> {
        match m {
            Message::FileStart { id, path, size } => {
                let rel = sanitize(path).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsicherer Pfad: {path}"),
                    )
                })?;
                let target = self.target_for(&rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let tmp = target.with_file_name(format!(
                    ".{}.part",
                    target.file_name().unwrap().to_string_lossy()
                ));
                let file = File::create(&tmp)?;
                self.open.insert(
                    *id,
                    Incoming {
                        file,
                        tmp,
                        target,
                        expected: *size,
                        written: 0,
                    },
                );
                Ok(None)
            }
            Message::FileChunk { id, data } => {
                let inc = self.open.get_mut(id).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "unbekannte Datei")
                })?;
                inc.written += data.len() as u64;
                if inc.written > inc.expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Datei größer als angekündigt",
                    ));
                }
                inc.file.write_all(data)?;
                self.received += data.len() as u64;
                Ok(Some(ReceiveEvent::Progress {
                    received: self.received,
                }))
            }
            Message::FileEnd { id } => {
                if let Some(mut inc) = self.open.remove(id) {
                    inc.file.flush()?;
                    drop(inc.file);
                    fs::rename(&inc.tmp, &inc.target)?;
                    self.finished.push(inc.target);
                }
                Ok(None)
            }
            Message::BatchEnd { open_first, .. } => {
                for (_, inc) in self.open.drain() {
                    let _ = fs::remove_file(inc.tmp);
                }
                self.roots.clear();
                self.received = 0;
                let files = std::mem::take(&mut self.finished);
                Ok(Some(ReceiveEvent::Done {
                    files,
                    open_first: *open_first,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Verbindung abgebrochen: halbe Dateien wegräumen.
    pub fn abort(&mut self) {
        for (_, inc) in self.open.drain() {
            let _ = fs::remove_file(inc.tmp);
        }
        self.roots.clear();
        self.finished.clear();
        self.received = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sanitize_rejects_escapes() {
        assert!(sanitize("../x").is_none());
        assert!(sanitize("a/../../x").is_none());
        assert!(sanitize("/etc/passwd").is_none());
        assert!(sanitize("C:\\Windows").is_none());
        assert!(sanitize("CON.txt").is_none());
        assert_eq!(
            sanitize("Ordner/bild.png"),
            Some(PathBuf::from("Ordner").join("bild.png"))
        );
    }

    #[test]
    fn send_and_receive_folder() {
        let src = tmpdir("src");
        fs::create_dir_all(src.join("Projekt/sub")).unwrap();
        fs::write(src.join("Projekt/a.txt"), b"hallo").unwrap();
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(src.join("Projekt/sub/b.bin"), &big).unwrap();
        fs::write(src.join("Notiz.md"), b"# hi").unwrap();

        let items = collect(&[src.join("Projekt"), src.join("Notiz.md")]).unwrap();
        assert_eq!(items.len(), 3);

        let outbox = Outbox::new();
        let dst = tmpdir("dst");
        // Vorhandene Datei, damit ein eindeutiger Name gewählt wird.
        fs::write(dst.join("Notiz.md"), b"alt").unwrap();
        let mut rx = Receiver::new(dst.clone());

        std::thread::scope(|s| {
            s.spawn(|| send(&outbox, &items, 1, false, |_, _| {}).unwrap());
            loop {
                let m = outbox.pop(Duration::from_secs(5)).expect("Nachricht");
                if let Some(ReceiveEvent::Done { files, .. }) = rx.handle(&m).unwrap() {
                    assert_eq!(files.len(), 3);
                    break;
                }
            }
        });

        assert_eq!(fs::read(dst.join("Projekt/a.txt")).unwrap(), b"hallo");
        assert_eq!(fs::read(dst.join("Projekt/sub/b.bin")).unwrap(), big);
        assert_eq!(fs::read(dst.join("Notiz (2).md")).unwrap(), b"# hi");
        assert_eq!(fs::read(dst.join("Notiz.md")).unwrap(), b"alt");
    }

    #[test]
    fn oversized_file_is_rejected() {
        let dst = tmpdir("over");
        let mut rx = Receiver::new(dst);
        rx.handle(&Message::FileStart {
            id: 1,
            path: "x".into(),
            size: 2,
        })
        .unwrap();
        assert!(rx
            .handle(&Message::FileChunk {
                id: 1,
                data: vec![0; 3]
            })
            .is_err());
    }

    #[test]
    fn top_level_and_move() {
        let base = tmpdir("top");
        let dest = tmpdir("top-dest");
        fs::create_dir_all(base.join("Ordner/sub")).unwrap();
        fs::write(base.join("Ordner/sub/a.txt"), b"a").unwrap();
        fs::write(base.join("Ordner/b.txt"), b"b").unwrap();
        fs::write(base.join("c.txt"), b"c").unwrap();
        let files = vec![
            base.join("Ordner/sub/a.txt"),
            base.join("Ordner/b.txt"),
            base.join("c.txt"),
        ];
        let top = top_level(&files, &base);
        assert_eq!(top, vec![base.join("Ordner"), base.join("c.txt")]);
        fs::write(dest.join("c.txt"), b"alt").unwrap();
        let moved: Vec<_> = top.iter().map(|t| move_into(t, &dest).unwrap()).collect();
        assert_eq!(moved[1], dest.join("c (2).txt"));
        assert_eq!(fs::read(dest.join("Ordner/sub/a.txt")).unwrap(), b"a");
        assert!(!base.join("Ordner").exists());
    }
}
