//! Laufwerke des anderen Rechners: Dateizugriff über die Verbindung.
//!
//! Der eine Rechner fragt ([`FsOp`]), der andere antwortet ([`FsData`] oder
//! [`FsError`]). Pfade sind virtuell: `Volume/Ordner/Datei`, getrennt mit
//! `/`. Die oberste Ebene sind die Laufwerke des antwortenden Rechners
//! (z. B. `Macintosh HD`, `USB-Stick` oder `Windows (C)`).
//!
//! [`serve`] führt eine Anfrage gegen die lokalen Laufwerke aus und ist
//! plattformunabhängig testbar.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// Größter Block pro Lese- oder Schreibanfrage. Klein genug, dass ein Block
/// über Bluetooth Eingaben nicht spürbar aufhält (wie [`crate::files::CHUNK`]).
pub const MAX_IO: u32 = 16 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FsOp {
    /// Die Laufwerke (oberste Ebene).
    Volumes,
    Stat {
        path: String,
    },
    /// Inhalt eines Ordners.
    List {
        path: String,
    },
    Read {
        path: String,
        offset: u64,
        len: u32,
    },
    /// `truncate`: Datei neu anlegen bzw. leeren, bevor geschrieben wird.
    Write {
        path: String,
        offset: u64,
        data: Vec<u8>,
        truncate: bool,
    },
    Mkdir {
        path: String,
    },
    /// Datei oder Ordner samt Inhalt löschen.
    Remove {
        path: String,
    },
    Rename {
        from: String,
        to: String,
        overwrite: bool,
    },
    Copy {
        from: String,
        to: String,
        overwrite: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Sekunden seit 1970 (UTC).
    pub modified: i64,
    pub created: i64,
    pub readonly: bool,
    pub hidden: bool,
    /// Nur bei Laufwerken: Gesamt- und freier Platz in Bytes.
    pub total: Option<u64>,
    pub free: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FsData {
    Entry(FsEntry),
    Entries(Vec<FsEntry>),
    Data(Vec<u8>),
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsErrorKind {
    NotFound,
    Denied,
    Exists,
    NotEmpty,
    /// Ungültiger Name oder Pfad.
    Invalid,
    /// Freigabe am anderen Rechner ausgeschaltet.
    Disabled,
    /// Keine Verbindung oder keine Antwort.
    Unavailable,
    Other,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FsError {
    pub kind: FsErrorKind,
    pub message: String,
}

impl FsError {
    pub fn new(kind: FsErrorKind, message: impl Into<String>) -> FsError {
        FsError {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> FsError {
        let kind = match e.kind() {
            io::ErrorKind::NotFound => FsErrorKind::NotFound,
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
                FsErrorKind::Denied
            }
            io::ErrorKind::AlreadyExists => FsErrorKind::Exists,
            io::ErrorKind::DirectoryNotEmpty => FsErrorKind::NotEmpty,
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidFilename => FsErrorKind::Invalid,
            _ => FsErrorKind::Other,
        };
        FsError::new(kind, e.to_string())
    }
}

pub type FsResult = Result<FsData, FsError>;

/// Ein freigegebenes Laufwerk dieses Rechners.
#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    /// Anzeigename, eindeutig, ohne `/`.
    pub name: String,
    pub root: PathBuf,
    pub total: Option<u64>,
    pub free: Option<u64>,
}

/// Wohin ein virtueller Pfad zeigt.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// Die Laufwerksliste.
    Top,
    /// Wurzel eines Laufwerks (Index in der Liste).
    VolumeRoot(usize),
    /// Etwas innerhalb eines Laufwerks.
    Inside(usize, PathBuf),
}

impl Resolved {
    fn path<'a>(&'a self, vols: &'a [Volume]) -> Option<&'a Path> {
        match self {
            Resolved::Top => None,
            Resolved::VolumeRoot(i) => Some(&vols[*i].root),
            Resolved::Inside(_, p) => Some(p),
        }
    }
}

fn valid_component(c: &str) -> bool {
    !(c == "."
        || c == ".."
        || c.contains('\\')
        || c.contains('\0')
        || (cfg!(windows) && c.contains(':')))
}

/// Namen anhängen. macOS schickt Umlaute zerlegt (NFD), Windows speichert
/// sie meist zusammengesetzt (NFC): Gibt es den Namen so nicht, wird ein
/// gleichwertiger gesucht; neue Namen werden zusammengesetzt angelegt.
fn push_component(p: &mut PathBuf, c: &str) {
    if c.is_ascii() || fs::symlink_metadata(p.join(c)).is_ok() {
        p.push(c);
        return;
    }
    let want: String = c.nfc().collect();
    if let Ok(rd) = fs::read_dir(&*p) {
        for e in rd.flatten() {
            let name = e.file_name();
            if let Some(n) = name.to_str() {
                if n.nfc().eq(want.chars()) {
                    p.push(n);
                    return;
                }
            }
        }
    }
    p.push(want);
}

/// Virtuellen Pfad in einen echten übersetzen. Nie außerhalb eines Laufwerks.
pub fn resolve(vols: &[Volume], vpath: &str) -> Result<Resolved, FsError> {
    let mut parts = vpath.split('/').filter(|c| !c.is_empty());
    let Some(vol) = parts.next() else {
        return Ok(Resolved::Top);
    };
    let idx = vols
        .iter()
        .position(|v| v.name == vol)
        .ok_or_else(|| FsError::new(FsErrorKind::NotFound, format!("Laufwerk {vol}")))?;
    let mut p = vols[idx].root.clone();
    let mut inside = false;
    for c in parts {
        if !valid_component(c) {
            return Err(FsError::new(FsErrorKind::Invalid, format!("Name {c:?}")));
        }
        push_component(&mut p, c);
        inside = true;
    }
    Ok(if inside {
        Resolved::Inside(idx, p)
    } else {
        Resolved::VolumeRoot(idx)
    })
}

fn secs(t: io::Result<SystemTime>) -> i64 {
    match t {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        },
        Err(_) => 0,
    }
}

fn is_hidden(name: &str, _md: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        if _md.file_attributes() & HIDDEN != 0 {
            return true;
        }
    }
    name.starts_with('.')
}

fn entry(name: String, p: &Path) -> io::Result<FsEntry> {
    // Kaputte Verknüpfungen trotzdem zeigen statt die Liste scheitern zu lassen.
    let md = fs::metadata(p).or_else(|_| fs::symlink_metadata(p))?;
    Ok(FsEntry {
        hidden: is_hidden(&name, &md),
        name,
        dir: md.is_dir(),
        size: if md.is_dir() { 0 } else { md.len() },
        modified: secs(md.modified()),
        created: secs(md.created()),
        readonly: md.permissions().readonly(),
        total: None,
        free: None,
    })
}

fn volume_entry(v: &Volume) -> FsEntry {
    let md = fs::metadata(&v.root).ok();
    FsEntry {
        name: v.name.clone(),
        dir: true,
        size: 0,
        modified: md.as_ref().map(|m| secs(m.modified())).unwrap_or(0),
        created: md.as_ref().map(|m| secs(m.created())).unwrap_or(0),
        readonly: false,
        hidden: false,
        total: v.total,
        free: v.free,
    }
}

fn top_entry() -> FsEntry {
    FsEntry {
        name: String::new(),
        dir: true,
        size: 0,
        modified: 0,
        created: 0,
        readonly: true,
        hidden: false,
        total: None,
        free: None,
    }
}

fn need_inside(r: &Resolved, what: &str) -> Result<PathBuf, FsError> {
    match r {
        Resolved::Inside(_, p) => Ok(p.clone()),
        _ => Err(FsError::new(
            FsErrorKind::Denied,
            format!("{what} ist bei Laufwerken nicht möglich"),
        )),
    }
}

fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    if fs::metadata(from)?.is_dir() {
        fs::create_dir(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_recursive(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

/// Eine Anfrage gegen die lokalen Laufwerke ausführen.
pub fn serve(vols: &[Volume], op: &FsOp) -> FsResult {
    match op {
        FsOp::Volumes => Ok(FsData::Entries(vols.iter().map(volume_entry).collect())),
        FsOp::Stat { path } => match resolve(vols, path)? {
            Resolved::Top => Ok(FsData::Entry(top_entry())),
            Resolved::VolumeRoot(i) => Ok(FsData::Entry(volume_entry(&vols[i]))),
            Resolved::Inside(_, p) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok(FsData::Entry(entry(name, &p)?))
            }
        },
        FsOp::List { path } => {
            let r = resolve(vols, path)?;
            let Some(dir) = r.path(vols) else {
                return Ok(FsData::Entries(vols.iter().map(volume_entry).collect()));
            };
            let mut out = Vec::new();
            for e in fs::read_dir(dir)? {
                let Ok(e) = e else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if let Ok(en) = entry(name, &e.path()) {
                    out.push(en);
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(FsData::Entries(out))
        }
        FsOp::Read { path, offset, len } => {
            let p = need_inside(&resolve(vols, path)?, "Lesen")?;
            let mut f = File::open(&p)?;
            f.seek(SeekFrom::Start(*offset))?;
            let want = (*len).min(MAX_IO) as usize;
            let mut buf = vec![0u8; want];
            let mut got = 0;
            while got < want {
                match f.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            buf.truncate(got);
            Ok(FsData::Data(buf))
        }
        FsOp::Write {
            path,
            offset,
            data,
            truncate,
        } => {
            let p = need_inside(&resolve(vols, path)?, "Schreiben")?;
            if p.is_dir() {
                return Err(FsError::new(FsErrorKind::Exists, "ist ein Ordner"));
            }
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(*truncate)
                .open(&p)?;
            f.seek(SeekFrom::Start(*offset))?;
            f.write_all(data)?;
            Ok(FsData::Done)
        }
        FsOp::Mkdir { path } => {
            let p = need_inside(&resolve(vols, path)?, "Ordner anlegen")?;
            fs::create_dir(&p)?;
            Ok(FsData::Done)
        }
        FsOp::Remove { path } => {
            let p = need_inside(&resolve(vols, path)?, "Löschen")?;
            remove_any(&p)?;
            Ok(FsData::Done)
        }
        FsOp::Rename {
            from,
            to,
            overwrite,
        }
        | FsOp::Copy {
            from,
            to,
            overwrite,
        } => {
            let src = need_inside(&resolve(vols, from)?, "Verschieben")?;
            let dst = need_inside(&resolve(vols, to)?, "Verschieben")?;
            if src == dst {
                return Err(FsError::new(FsErrorKind::Denied, "Quelle gleich Ziel"));
            }
            if dst.starts_with(&src) {
                return Err(FsError::new(
                    FsErrorKind::Invalid,
                    "Ziel liegt in der Quelle",
                ));
            }
            fs::symlink_metadata(&src)?;
            if fs::symlink_metadata(&dst).is_ok() {
                if !overwrite {
                    return Err(FsError::new(FsErrorKind::Exists, "Ziel existiert"));
                }
                remove_any(&dst)?;
            }
            if matches!(op, FsOp::Copy { .. }) {
                copy_recursive(&src, &dst)?;
            } else if fs::rename(&src, &dst).is_err() {
                // Anderes Laufwerk: kopieren und Quelle entfernen.
                copy_recursive(&src, &dst)?;
                remove_any(&src)?;
            }
            Ok(FsData::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> (PathBuf, Vec<Volume>) {
        let d = std::env::temp_dir().join(format!("dc-rfs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a/sub")).unwrap();
        fs::create_dir_all(d.join("b")).unwrap();
        fs::write(d.join("a/hallo.txt"), b"Hallo Welt").unwrap();
        let vols = vec![
            Volume {
                name: "Platte A".into(),
                root: d.join("a"),
                total: Some(100),
                free: Some(40),
            },
            Volume {
                name: "B".into(),
                root: d.join("b"),
                total: None,
                free: None,
            },
        ];
        (d, vols)
    }

    fn entries(r: FsResult) -> Vec<FsEntry> {
        match r.unwrap() {
            FsData::Entries(e) => e,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lists_volumes_and_folders() {
        let (_d, v) = setup("list");
        let top = entries(serve(&v, &FsOp::List { path: "".into() }));
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "Platte A");
        assert_eq!(top[0].free, Some(40));
        let a = entries(serve(
            &v,
            &FsOp::List {
                path: "/Platte A/".into(),
            },
        ));
        let names: Vec<_> = a.iter().map(|e| (e.name.as_str(), e.dir, e.size)).collect();
        assert_eq!(names, vec![("hallo.txt", false, 10), ("sub", true, 0)]);
    }

    #[test]
    fn read_write_in_blocks() {
        let (d, v) = setup("rw");
        for (i, chunk) in [b"abc".as_slice(), b"def"].iter().enumerate() {
            serve(
                &v,
                &FsOp::Write {
                    path: "B/neu.bin".into(),
                    offset: i as u64 * 3,
                    data: chunk.to_vec(),
                    truncate: i == 0,
                },
            )
            .unwrap();
        }
        assert_eq!(fs::read(d.join("b/neu.bin")).unwrap(), b"abcdef");
        let r = serve(
            &v,
            &FsOp::Read {
                path: "B/neu.bin".into(),
                offset: 2,
                len: 3,
            },
        );
        assert_eq!(r.unwrap(), FsData::Data(b"cde".to_vec()));
        // Neu anlegen leert die Datei.
        serve(
            &v,
            &FsOp::Write {
                path: "B/neu.bin".into(),
                offset: 0,
                data: vec![],
                truncate: true,
            },
        )
        .unwrap();
        assert_eq!(fs::read(d.join("b/neu.bin")).unwrap(), b"");
    }

    #[test]
    fn rejects_escapes_and_volume_changes() {
        let (_d, v) = setup("esc");
        for p in ["B/../a/hallo.txt", "B/./x", "Gibtsnicht/x"] {
            assert!(
                serve(&v, &FsOp::Stat { path: p.into() }).is_err(),
                "{p} müsste scheitern"
            );
        }
        let e = serve(&v, &FsOp::Remove { path: "B".into() }).unwrap_err();
        assert_eq!(e.kind, FsErrorKind::Denied);
        let e = serve(
            &v,
            &FsOp::Read {
                path: "Platte A/fehlt".into(),
                offset: 0,
                len: 1,
            },
        )
        .unwrap_err();
        assert_eq!(e.kind, FsErrorKind::NotFound);
    }

    #[test]
    fn finds_names_in_other_unicode_form() {
        let (d, v) = setup("nfd");
        fs::write(d.join("b/B\u{fc}ro.txt"), b"x").unwrap();
        // Zerlegt angefragt (wie vom Finder), zusammengesetzt gespeichert.
        let r = serve(
            &v,
            &FsOp::Stat {
                path: "B/Bu\u{308}ro.txt".into(),
            },
        );
        assert!(r.is_ok(), "{r:?}");
        serve(
            &v,
            &FsOp::Mkdir {
                path: "B/Gru\u{308}n".into(),
            },
        )
        .unwrap();
        assert!(d.join("b/Gr\u{fc}n").is_dir());
    }

    #[test]
    fn rename_copy_across_volumes_and_overwrite() {
        let (d, v) = setup("mv");
        serve(
            &v,
            &FsOp::Copy {
                from: "Platte A/hallo.txt".into(),
                to: "B/kopie.txt".into(),
                overwrite: false,
            },
        )
        .unwrap();
        let e = serve(
            &v,
            &FsOp::Rename {
                from: "Platte A/hallo.txt".into(),
                to: "B/kopie.txt".into(),
                overwrite: false,
            },
        )
        .unwrap_err();
        assert_eq!(e.kind, FsErrorKind::Exists);
        serve(
            &v,
            &FsOp::Rename {
                from: "Platte A/sub".into(),
                to: "B/sub2".into(),
                overwrite: true,
            },
        )
        .unwrap();
        assert!(d.join("b/sub2").is_dir());
        assert!(!d.join("a/sub").exists());
        let e = serve(
            &v,
            &FsOp::Copy {
                from: "B/sub2".into(),
                to: "B/sub2/innen".into(),
                overwrite: true,
            },
        )
        .unwrap_err();
        assert_eq!(e.kind, FsErrorKind::Invalid);
        serve(&v, &FsOp::Mkdir { path: "B/x".into() }).unwrap();
        assert_eq!(
            serve(&v, &FsOp::Mkdir { path: "B/x".into() })
                .unwrap_err()
                .kind,
            FsErrorKind::Exists
        );
        serve(
            &v,
            &FsOp::Remove {
                path: "B/sub2".into(),
            },
        )
        .unwrap();
        assert!(!d.join("b/sub2").exists());
    }
}
