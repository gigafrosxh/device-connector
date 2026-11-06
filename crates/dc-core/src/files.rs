//! Dateiübertragung (Phase 2).
//!
//! Sender: Dateien und Ordner rekursiv einsammeln ([`collect`]), in Blöcken
//! über die niedrige Priorität der [`Outbox`] schicken ([`send`]). Jeder
//! Vorgang hat eine eigene Übertragungs-ID, damit gleichzeitige Vorgänge sich
//! nicht vermischen.
//!
//! Empfänger ([`Receiver`]): Pfade prüfen (kein `..`, keine absoluten Pfade,
//! keine reservierten Windows-Namen), keinen Symlinks folgen, Limits
//! durchsetzen, erst als `.part` schreiben und nur bei exakt passender Größe
//! sichtbar machen. Schlägt etwas fehl, wird der ganze Vorgang weggeräumt
//! und als [`ReceiveEvent::Failed`] gemeldet (der Dienst schickt das dem
//! Sender als [`Message::TransferResult`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::link::Outbox;
use crate::protocol::Message;

pub const CHUNK: usize = 16 * 1024;
/// Höchstzahl Dateien pro Vorgang (Sender und Empfänger).
pub const MAX_FILES: usize = 10_000;
/// Höchstzahl Ordner pro Vorgang.
const MAX_DIRS: usize = 100_000;
/// Gleichzeitig offene Vorgänge beim Empfänger.
const MAX_BATCHES: usize = 16;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_PATH_BYTES: usize = 4096;

/// Eine zu sendende Datei mit relativem Zielpfad (`/`-getrennt).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub source: PathBuf,
    pub rel: String,
    pub size: u64,
}

/// Ergebnis von [`collect`]: was gesendet wird und was nicht (mit Grund).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Collection {
    pub items: Vec<Item>,
    /// Relative Ordner (`/`-getrennt), Eltern vor Kindern.
    pub dirs: Vec<String>,
    /// Nicht übertragene Einträge mit Grund.
    pub skipped: Vec<(PathBuf, String)>,
}

impl Collection {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.dirs.is_empty()
    }
}

/// Dateien und Ordner einsammeln. Nicht lesbare Einträge und Verknüpfungen
/// landen in [`Collection::skipped`]; mehr als [`MAX_FILES`] Dateien sind ein
/// Fehler (statt stillschweigend abzuschneiden).
pub fn collect(paths: &[PathBuf]) -> io::Result<Collection> {
    let mut out = Collection::default();
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Datei".into());
        walk(p, &name, &mut out)?;
    }
    Ok(out)
}

fn too_many() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("zu viele Dateien (höchstens {MAX_FILES} pro Vorgang)"),
    )
}

fn walk(path: &Path, rel: &str, out: &mut Collection) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            out.skipped.push((path.to_path_buf(), e.to_string()));
            return Ok(());
        }
    };
    if meta.is_symlink() {
        // Symlinks bewusst nicht folgen.
        out.skipped.push((
            path.to_path_buf(),
            "Verknüpfung wird nicht übertragen".into(),
        ));
        return Ok(());
    }
    if meta.is_dir() {
        if out.dirs.len() >= MAX_DIRS {
            return Err(too_many());
        }
        out.dirs.push(rel.to_string());
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(e) => {
                out.skipped.push((path.to_path_buf(), e.to_string()));
                return Ok(());
            }
        };
        let mut entries = Vec::new();
        for e in rd {
            match e {
                Ok(e) => entries.push(e),
                Err(err) => out.skipped.push((path.to_path_buf(), err.to_string())),
            }
        }
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            if name == ".DS_Store" || name == "Thumbs.db" || name == "desktop.ini" {
                continue;
            }
            walk(&e.path(), &format!("{rel}/{name}"), out)?;
        }
    } else if meta.is_file() {
        if out.items.len() >= MAX_FILES {
            return Err(too_many());
        }
        out.items.push(Item {
            source: path.to_path_buf(),
            rel: rel.to_string(),
            size: meta.len(),
        });
    } else {
        out.skipped
            .push((path.to_path_buf(), "keine normale Datei".into()));
    }
    Ok(())
}

/// Sendet einen Vorgang; blockiert, bis alles in der Warteschlange ist.
/// Fortschritt: `progress(gesendete_bytes, gesamt_bytes)`. `cancel` bricht ab
/// (z. B. wenn der Empfänger abgelehnt hat).
pub fn send(
    outbox: &Outbox,
    c: &Collection,
    transfer: u32,
    open_first: bool,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<()> {
    let total = c.total_bytes();
    let mut done = 0u64;
    let closed = || io::Error::new(io::ErrorKind::BrokenPipe, "Verbindung getrennt");
    let cancelled = || io::Error::new(io::ErrorKind::Interrupted, "abgebrochen");
    let push = |m: Message| -> io::Result<()> {
        if cancel.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        if outbox.push_bulk(m) {
            Ok(())
        } else {
            Err(closed())
        }
    };
    for d in &c.dirs {
        push(Message::Dir {
            transfer,
            path: d.clone(),
        })?;
    }
    for (n, item) in c.items.iter().enumerate() {
        let id = n as u32;
        let f = File::open(&item.source)?;
        push(Message::FileStart {
            transfer,
            id,
            path: item.rel.clone(),
            size: item.size,
        })?;
        // Nie mehr als angekündigt senden; wird die Datei unterwegs kürzer,
        // merkt der Empfänger das an der Größe und lehnt ab.
        let mut f = f.take(item.size);
        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0u64;
        loop {
            let k = match f.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = push(Message::FileEnd { transfer, id });
                    return Err(e);
                }
            };
            push(Message::FileChunk {
                transfer,
                id,
                data: buf[..k].to_vec(),
            })?;
            sent += k as u64;
            done += k as u64;
            progress(done, total);
        }
        push(Message::FileEnd { transfer, id })?;
        if sent != item.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} hat sich beim Lesen verändert", item.source.display()),
            ));
        }
    }
    push(Message::BatchEnd {
        transfer,
        files: c.items.len() as u32,
        open_first,
    })
}

/// Unter Windows reservierte Gerätenamen (auch mit Endung, z. B. `NUL.txt`).
fn is_reserved_windows_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return true;
    }
    let mut chars = stem.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let rest: Vec<char> = chars.collect();
    (prefix == "COM" || prefix == "LPT")
        && rest.len() == 1
        && (rest[0].is_ascii_digit() || matches!(rest[0], '¹' | '²' | '³'))
}

/// Wandelt einen empfangenen Pfad in einen sicheren relativen Pfad um.
/// Unter Windows ungültige Zeichen (`<>"|?*`) werden durch `_` ersetzt,
/// Punkte/Leerzeichen am Ende entfernt (Windows tut das ohnehin), und erst
/// danach wird geprüft.
pub fn sanitize(rel: &str) -> Option<PathBuf> {
    if rel.len() > MAX_PATH_BYTES {
        return None;
    }
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        if part.contains(['\\', ':', '\0']) || part.chars().any(|c| c.is_control()) {
            return None;
        }
        let replaced: String = part
            .chars()
            .map(|c| if "<>\"|?*".contains(c) { '_' } else { c })
            .collect();
        let name = replaced.trim_end_matches([' ', '.']);
        if name.is_empty() || name.len() > MAX_COMPONENT_BYTES {
            return None;
        }
        if is_reserved_windows_name(name) {
            return None;
        }
        let p = Path::new(name);
        if p.components().count() != 1
            || !matches!(p.components().next(), Some(Component::Normal(_)))
        {
            return None;
        }
        out.push(name);
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Existiert hier schon etwas (auch ein kaputter Symlink)?
fn occupied(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Freien Namen finden: „Bericht.pdf“ → „Bericht (2).pdf“, wenn es schon existiert.
pub fn unique(path: PathBuf) -> PathBuf {
    if !occupied(&path) {
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
    for n in 2..100_000 {
        let candidate = parent.join(format!("{stem} ({n}){ext}"));
        if !occupied(&candidate) {
            return candidate;
        }
    }
    parent.join(format!("{stem} ({}){ext}", rand::random::<u32>()))
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

/// Grenzen für empfangene Vorgänge (Schutz vor vollen Platten und Missbrauch).
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_files: u32,
    /// So viel muss nach dem Empfang mindestens frei bleiben.
    pub keep_free_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        Limits {
            max_file_bytes: 64 * GIB,
            max_total_bytes: 256 * GIB,
            max_files: MAX_FILES as u32,
            keep_free_bytes: GIB / 2,
        }
    }
}

/// Freier Platz am Pfad, falls die Plattform ihn kennt.
pub type FreeSpaceProbe = fn(&Path) -> Option<u64>;

#[derive(Debug, PartialEq)]
pub enum ReceiveEvent {
    Progress {
        transfer: u32,
        received: u64,
    },
    /// Vorgang vollständig und geprüft: alle Dateien (erste zuerst) und Ordner.
    Done {
        transfer: u32,
        base: PathBuf,
        files: Vec<PathBuf>,
        dirs: Vec<PathBuf>,
        open_first: bool,
    },
    /// Vorgang abgelehnt; alles, was er angelegt hatte, ist wieder weg.
    Failed {
        transfer: u32,
        error: String,
    },
}

struct Incoming {
    file: File,
    tmp: PathBuf,
    target: PathBuf,
    expected: u64,
    written: u64,
}

struct Batch {
    base: PathBuf,
    /// Abbildung oberster Name → tatsächlicher (eindeutiger) Name in diesem Vorgang.
    roots: HashMap<String, PathBuf>,
    open: HashMap<u32, Incoming>,
    seen_ids: HashSet<u32>,
    finished: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    /// Von diesem Vorgang angelegte Ordner (zum Aufräumen bei Fehlern).
    created: Vec<PathBuf>,
    received: u64,
    announced: u64,
}

type Fail = String;

impl Batch {
    fn new(base: PathBuf) -> io::Result<Batch> {
        fs::create_dir_all(&base)?;
        let base = fs::canonicalize(&base)?;
        Ok(Batch {
            base,
            roots: HashMap::new(),
            open: HashMap::new(),
            seen_ids: HashSet::new(),
            finished: Vec::new(),
            dirs: Vec::new(),
            created: Vec::new(),
            received: 0,
            announced: 0,
        })
    }

    fn target_for(&mut self, rel: &Path) -> PathBuf {
        let mut comps = rel.components();
        let root = comps
            .next()
            .expect("sanitize liefert nie leere Pfade")
            .as_os_str()
            .to_string_lossy()
            .to_string();
        let rest: PathBuf = comps.collect();
        let base = self.base.clone();
        let root_path = self
            .roots
            .entry(root)
            .or_insert_with(|| unique(base.join(rel.components().next().unwrap())))
            .clone();
        if rest.as_os_str().is_empty() {
            root_path
        } else {
            root_path.join(rest)
        }
    }

    /// Legt `dir` (unterhalb von `base`) an, ohne Symlinks zu folgen.
    fn ensure_dir(&mut self, dir: &Path) -> Result<(), Fail> {
        let rel = dir
            .strip_prefix(&self.base)
            .map_err(|_| "Pfad außerhalb des Empfangsordners".to_string())?;
        let mut cur = self.base.clone();
        for c in rel.components() {
            cur.push(c);
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.is_symlink() => {
                    return Err(format!("{}: Verknüpfung im Zielpfad", cur.display()))
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(format!("{}: ist kein Ordner", cur.display())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&cur).map_err(|e| format!("{}: {e}", cur.display()))?;
                    self.created.push(cur.clone());
                }
                Err(e) => return Err(format!("{}: {e}", cur.display())),
            }
        }
        // Zur Sicherheit: tatsächlicher Ort muss im Empfangsordner liegen.
        let real = fs::canonicalize(dir).map_err(|e| e.to_string())?;
        if !real.starts_with(&self.base) {
            return Err("Pfad außerhalb des Empfangsordners".into());
        }
        Ok(())
    }

    /// Alles wegräumen, was dieser Vorgang angelegt hat.
    fn discard(mut self) {
        for (_, inc) in self.open.drain() {
            drop(inc.file);
            let _ = fs::remove_file(inc.tmp);
        }
        for f in &self.finished {
            let _ = fs::remove_file(f);
        }
        for d in self.created.iter().rev() {
            let _ = fs::remove_dir(d); // nur, wenn leer
        }
    }
}

/// `.part`-Datei exklusiv anlegen (folgt keinem Symlink, überschreibt nichts).
fn create_part(target: &Path, transfer: u32, id: u32) -> io::Result<(File, PathBuf)> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir = target.parent().unwrap_or(Path::new("."));
    for n in 0..100u32 {
        let tmp = dir.join(format!(".{name}.dc{transfer}-{id}-{n}.part"));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "kein freier Name für die Zwischendatei",
    ))
}

/// Fertige Datei sichtbar machen, ohne etwas zu überschreiben.
fn commit(tmp: &Path, target: &Path) -> io::Result<PathBuf> {
    let mut dest = target.to_path_buf();
    for _ in 0..3 {
        if occupied(&dest) {
            dest = unique(dest);
        }
        // hard_link scheitert, wenn das Ziel existiert: kein Überschreiben.
        match fs::hard_link(tmp, &dest) {
            Ok(()) => {
                fs::remove_file(tmp)?;
                return Ok(dest);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // Dateisystem ohne harte Links (FAT/exFAT): umbenennen.
                if !occupied(&dest) {
                    fs::rename(tmp, &dest)?;
                    return Ok(dest);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} existiert schon", target.display()),
    ))
}

pub struct Receiver {
    default_base: PathBuf,
    /// Vorab festgelegter Zielordner für einzelne Vorgänge (z. B. Ziehen).
    bases: HashMap<u32, PathBuf>,
    batches: HashMap<u32, Batch>,
    /// Abgelehnte Vorgänge: Nachzügler still verwerfen.
    failed: VecDeque<u32>,
    limits: Limits,
    free_space: Option<FreeSpaceProbe>,
}

impl Receiver {
    pub fn new(base: PathBuf) -> Receiver {
        Receiver {
            default_base: base,
            bases: HashMap::new(),
            batches: HashMap::new(),
            failed: VecDeque::new(),
            limits: Limits::default(),
            free_space: None,
        }
    }

    pub fn base(&self) -> &Path {
        &self.default_base
    }

    pub fn set_base(&mut self, base: PathBuf) {
        self.default_base = base;
    }

    /// Zielordner nur für Vorgang `transfer` (z. B. verdeckter Ordner beim Ziehen).
    pub fn set_base_for(&mut self, transfer: u32, base: PathBuf) {
        if self.bases.len() < MAX_BATCHES {
            self.bases.insert(transfer, base);
        }
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    pub fn set_free_space_probe(&mut self, probe: FreeSpaceProbe) {
        self.free_space = Some(probe);
    }

    /// Läuft gerade ein Vorgang?
    pub fn busy(&self) -> bool {
        !self.batches.is_empty()
    }

    fn fail(&mut self, transfer: u32, error: String) -> Option<ReceiveEvent> {
        if let Some(b) = self.batches.remove(&transfer) {
            b.discard();
        }
        self.bases.remove(&transfer);
        if !self.failed.contains(&transfer) {
            if self.failed.len() >= 64 {
                self.failed.pop_front();
            }
            self.failed.push_back(transfer);
        }
        Some(ReceiveEvent::Failed { transfer, error })
    }

    fn batch(&mut self, transfer: u32) -> Result<&mut Batch, Fail> {
        if !self.batches.contains_key(&transfer) {
            if self.batches.len() >= MAX_BATCHES {
                return Err("zu viele gleichzeitige Übertragungen".into());
            }
            let base = self
                .bases
                .remove(&transfer)
                .unwrap_or_else(|| self.default_base.clone());
            let b = Batch::new(base).map_err(|e| format!("Empfangsordner: {e}"))?;
            self.batches.insert(transfer, b);
        }
        Ok(self.batches.get_mut(&transfer).unwrap())
    }

    pub fn handle(&mut self, m: &Message) -> Option<ReceiveEvent> {
        let transfer = match m {
            Message::FileStart { transfer, .. }
            | Message::FileChunk { transfer, .. }
            | Message::FileEnd { transfer, .. }
            | Message::Dir { transfer, .. }
            | Message::BatchEnd { transfer, .. } => *transfer,
            _ => return None,
        };
        if self.failed.contains(&transfer) {
            if matches!(m, Message::BatchEnd { .. }) {
                self.failed.retain(|t| *t != transfer);
            }
            return None;
        }
        match self.step(transfer, m) {
            Ok(ev) => ev,
            Err(e) => self.fail(transfer, e),
        }
    }

    fn step(&mut self, transfer: u32, m: &Message) -> Result<Option<ReceiveEvent>, Fail> {
        let limits = self.limits.clone();
        let probe = self.free_space;
        match m {
            Message::Dir { path, .. } => {
                let rel = sanitize(path).ok_or_else(|| format!("unsicherer Pfad: {path}"))?;
                let b = self.batch(transfer)?;
                if b.dirs.len() >= MAX_DIRS {
                    return Err("zu viele Ordner".into());
                }
                let target = b.target_for(&rel);
                b.ensure_dir(&target)?;
                b.dirs.push(target);
                Ok(None)
            }
            Message::FileStart { id, path, size, .. } => {
                let rel = sanitize(path).ok_or_else(|| format!("unsicherer Pfad: {path}"))?;
                let b = self.batch(transfer)?;
                if !b.seen_ids.insert(*id) {
                    return Err(format!("Datei-ID {id} doppelt"));
                }
                if b.seen_ids.len() > limits.max_files as usize {
                    return Err(format!(
                        "zu viele Dateien (höchstens {} pro Vorgang)",
                        limits.max_files
                    ));
                }
                if *size > limits.max_file_bytes {
                    return Err(format!("{path}: Datei zu groß"));
                }
                b.announced = b
                    .announced
                    .checked_add(*size)
                    .filter(|t| *t <= limits.max_total_bytes)
                    .ok_or_else(|| "Übertragung zu groß".to_string())?;
                if let Some(free) = probe.and_then(|p| p(&b.base)) {
                    if free < size.saturating_add(limits.keep_free_bytes) {
                        return Err("nicht genug Speicherplatz".into());
                    }
                }
                let target = b.target_for(&rel);
                if let Some(parent) = target.parent() {
                    b.ensure_dir(parent)?;
                }
                let (file, tmp) =
                    create_part(&target, transfer, *id).map_err(|e| format!("{path}: {e}"))?;
                b.open.insert(
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
            Message::FileChunk { id, data, .. } => {
                let b = self
                    .batches
                    .get_mut(&transfer)
                    .ok_or("unbekannte Übertragung")?;
                let inc = b.open.get_mut(id).ok_or("unbekannte Datei")?;
                inc.written = inc
                    .written
                    .checked_add(data.len() as u64)
                    .filter(|w| *w <= inc.expected)
                    .ok_or("Datei größer als angekündigt")?;
                inc.file.write_all(data).map_err(|e| e.to_string())?;
                b.received += data.len() as u64;
                Ok(Some(ReceiveEvent::Progress {
                    transfer,
                    received: b.received,
                }))
            }
            Message::FileEnd { id, .. } => {
                let b = self
                    .batches
                    .get_mut(&transfer)
                    .ok_or("unbekannte Übertragung")?;
                let mut inc = b.open.remove(id).ok_or("unbekannte Datei")?;
                if inc.written != inc.expected {
                    let _ = fs::remove_file(&inc.tmp);
                    return Err(format!(
                        "{}: {} von {} Bytes angekommen",
                        inc.target.display(),
                        inc.written,
                        inc.expected
                    ));
                }
                let res = inc.file.flush();
                drop(inc.file);
                if let Err(e) = res {
                    let _ = fs::remove_file(&inc.tmp);
                    return Err(e.to_string());
                }
                match commit(&inc.tmp, &inc.target) {
                    Ok(p) => {
                        b.finished.push(p);
                        Ok(None)
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&inc.tmp);
                        Err(e.to_string())
                    }
                }
            }
            Message::BatchEnd {
                files, open_first, ..
            } => {
                let b = match self.batches.remove(&transfer) {
                    Some(b) => b,
                    None if *files == 0 => {
                        return Ok(Some(ReceiveEvent::Done {
                            transfer,
                            base: self.default_base.clone(),
                            files: Vec::new(),
                            dirs: Vec::new(),
                            open_first: *open_first,
                        }))
                    }
                    None => return Err("unbekannte Übertragung".into()),
                };
                if !b.open.is_empty() || b.finished.len() != *files as usize {
                    let msg = format!(
                        "unvollständig: {} von {} Dateien fertig",
                        b.finished.len(),
                        files
                    );
                    self.batches.insert(transfer, b);
                    return Err(msg);
                }
                Ok(Some(ReceiveEvent::Done {
                    transfer,
                    base: b.base,
                    files: b.finished,
                    dirs: b.dirs,
                    open_first: *open_first,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Vorgang ohne Fehlermeldung verwerfen (z. B. Vorab-Übertragung, die
    /// nicht gebraucht wird); Nachzügler werden ignoriert.
    pub fn discard(&mut self, transfer: u32) {
        let _ = self.fail(transfer, String::new());
    }

    /// Verbindung abgebrochen: halbe Vorgänge komplett wegräumen.
    pub fn abort(&mut self) {
        for (_, b) in self.batches.drain() {
            b.discard();
        }
        self.bases.clear();
        self.failed.clear();
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
        fs::canonicalize(d).unwrap()
    }

    fn start(transfer: u32, id: u32, path: &str, size: u64) -> Message {
        Message::FileStart {
            transfer,
            id,
            path: path.into(),
            size,
        }
    }
    fn chunk(transfer: u32, id: u32, data: &[u8]) -> Message {
        Message::FileChunk {
            transfer,
            id,
            data: data.to_vec(),
        }
    }
    fn end(transfer: u32, id: u32) -> Message {
        Message::FileEnd { transfer, id }
    }
    fn batch_end(transfer: u32, files: u32) -> Message {
        Message::BatchEnd {
            transfer,
            files,
            open_first: false,
        }
    }
    fn failed(ev: Option<ReceiveEvent>) -> bool {
        matches!(ev, Some(ReceiveEvent::Failed { .. }))
    }
    /// Alle Dateien unterhalb von `d` (rekursiv), relativ.
    fn tree(d: &Path) -> Vec<String> {
        fn go(base: &Path, d: &Path, out: &mut Vec<String>) {
            for e in fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                if p.is_dir() {
                    go(base, &p, out);
                }
            }
        }
        let mut v = Vec::new();
        go(d, d, &mut v);
        v.sort();
        v
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
    fn sanitize_windows_reserved_and_empty_components() {
        // #13
        for bad in [
            "...",
            ". .",
            "  ",
            "a/../b",
            "a/ . /b",
            "COM3",
            "com9.log",
            "LPT5.txt",
            "Ordner/nul",
            "aux .txt",
            "CONIN$",
            "conout$.x",
            "CLOCK$",
            "COM¹",
            "lpt².txt",
            "COM0",
            "x\u{1}y",
        ] {
            assert!(sanitize(bad).is_none(), "{bad:?} müsste abgelehnt werden");
        }
        for (ok, want) in [
            ("COM10.txt", "COM10.txt"),
            ("CONSOLE.txt", "CONSOLE.txt"),
            ("Grüße 👋.md", "Grüße 👋.md"),
            ("bericht.tar.gz", "bericht.tar.gz"),
            ("Notiz. ", "Notiz"),
            ("Was?.txt", "Was_.txt"),
            ("a<b>|c*.txt", "a_b__c_.txt"),
            ("Ordner./x", "Ordner/x"),
        ] {
            let want: PathBuf = want.split('/').collect();
            assert_eq!(sanitize(ok), Some(want), "{ok:?}");
        }
        assert!(sanitize(&"a".repeat(256)).is_none());
    }

    #[test]
    fn send_and_receive_folder() {
        let src = tmpdir("src");
        fs::create_dir_all(src.join("Projekt/sub")).unwrap();
        fs::create_dir_all(src.join("Projekt/leer/tief")).unwrap();
        fs::write(src.join("Projekt/a.txt"), b"hallo").unwrap();
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(src.join("Projekt/sub/b.bin"), &big).unwrap();
        fs::write(src.join("Notiz.md"), b"# hi").unwrap();
        fs::create_dir_all(src.join("NurLeer")).unwrap();

        let c = collect(&[
            src.join("Projekt"),
            src.join("Notiz.md"),
            src.join("NurLeer"),
        ])
        .unwrap();
        assert_eq!(c.items.len(), 3);
        assert!(c.skipped.is_empty());

        let outbox = Outbox::new();
        let dst = tmpdir("dst");
        // Vorhandene Datei, damit ein eindeutiger Name gewählt wird.
        fs::write(dst.join("Notiz.md"), b"alt").unwrap();
        let mut rx = Receiver::new(dst.clone());
        let cancel = AtomicBool::new(false);

        std::thread::scope(|s| {
            s.spawn(|| send(&outbox, &c, 9, false, &cancel, |_, _| {}).unwrap());
            loop {
                let m = outbox.pop(Duration::from_secs(5)).expect("Nachricht");
                match rx.handle(&m) {
                    Some(ReceiveEvent::Done {
                        files, transfer, ..
                    }) => {
                        assert_eq!(transfer, 9);
                        assert_eq!(files.len(), 3);
                        break;
                    }
                    Some(ReceiveEvent::Failed { error, .. }) => panic!("{error}"),
                    _ => {}
                }
            }
        });

        assert_eq!(fs::read(dst.join("Projekt/a.txt")).unwrap(), b"hallo");
        assert_eq!(fs::read(dst.join("Projekt/sub/b.bin")).unwrap(), big);
        assert_eq!(fs::read(dst.join("Notiz (2).md")).unwrap(), b"# hi");
        assert_eq!(fs::read(dst.join("Notiz.md")).unwrap(), b"alt");
        // #16: leere Ordner bleiben erhalten, auch ganz oben.
        assert!(dst.join("Projekt/leer/tief").is_dir());
        assert!(dst.join("NurLeer").is_dir());
        assert!(!rx.busy());
    }

    #[test]
    fn oversized_file_is_rejected() {
        let dst = tmpdir("over");
        let mut rx = Receiver::new(dst.clone());
        assert_eq!(rx.handle(&start(1, 1, "x", 2)), None);
        assert!(failed(rx.handle(&chunk(1, 1, &[0; 3]))));
        assert!(tree(&dst).is_empty(), "keine Reste: {:?}", tree(&dst));
    }

    #[test]
    fn truncated_file_is_not_published() {
        // #7: FileStart(size=10), 3 Bytes, FileEnd → abgelehnt, nichts sichtbar.
        let dst = tmpdir("trunc");
        let mut rx = Receiver::new(dst.clone());
        rx.handle(&start(1, 1, "x.bin", 10));
        rx.handle(&chunk(1, 1, b"abc"));
        assert!(failed(rx.handle(&end(1, 1))));
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));
        // Nachzügler des abgelehnten Vorgangs werden still verworfen.
        assert_eq!(rx.handle(&batch_end(1, 1)), None);
    }

    #[test]
    fn zero_byte_and_exact_files_succeed() {
        let dst = tmpdir("zero");
        let mut rx = Receiver::new(dst.clone());
        rx.handle(&start(1, 0, "leer.txt", 0));
        assert_eq!(rx.handle(&end(1, 0)), None);
        rx.handle(&start(1, 1, "drei.txt", 3));
        rx.handle(&chunk(1, 1, b"abc"));
        assert_eq!(rx.handle(&end(1, 1)), None);
        assert!(matches!(
            rx.handle(&batch_end(1, 2)),
            Some(ReceiveEvent::Done { files, .. }) if files.len() == 2
        ));
        assert_eq!(fs::read(dst.join("leer.txt")).unwrap(), b"");
        assert_eq!(fs::read(dst.join("drei.txt")).unwrap(), b"abc");
    }

    #[test]
    fn batch_end_requires_all_files() {
        // #8: fehlendes FileEnd oder falsche Anzahl → Fehler statt Done.
        let dst = tmpdir("batch");
        let mut rx = Receiver::new(dst.clone());
        rx.handle(&start(1, 0, "a", 1));
        rx.handle(&chunk(1, 0, b"a"));
        assert!(failed(rx.handle(&batch_end(1, 1))));
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));

        rx.handle(&start(2, 0, "b", 1));
        rx.handle(&chunk(2, 0, b"b"));
        rx.handle(&end(2, 0));
        assert!(failed(rx.handle(&batch_end(2, 3))));
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));

        // Leerer Vorgang ist in Ordnung.
        assert!(matches!(
            rx.handle(&batch_end(3, 0)),
            Some(ReceiveEvent::Done { files, .. }) if files.is_empty()
        ));
    }

    #[test]
    fn duplicate_ids_and_same_names_do_not_clobber() {
        // #9
        let dst = tmpdir("dup");
        let mut rx = Receiver::new(dst.clone());
        rx.handle(&start(1, 1, "a.txt", 3));
        rx.handle(&chunk(1, 1, b"123"));
        assert!(failed(rx.handle(&start(1, 1, "b.txt", 3))));
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));

        // Gleicher Name zweimal im selben Vorgang: beide bleiben erhalten.
        rx.handle(&start(2, 1, "Ordner/x.txt", 1));
        rx.handle(&start(2, 2, "Ordner/x.txt", 1));
        rx.handle(&chunk(2, 1, b"1"));
        rx.handle(&chunk(2, 2, b"2"));
        rx.handle(&end(2, 1));
        rx.handle(&end(2, 2));
        assert!(matches!(
            rx.handle(&batch_end(2, 2)),
            Some(ReceiveEvent::Done { .. })
        ));
        assert_eq!(fs::read(dst.join("Ordner/x.txt")).unwrap(), b"1");
        assert_eq!(fs::read(dst.join("Ordner/x (2).txt")).unwrap(), b"2");
    }

    #[test]
    fn interleaved_transfers_stay_separate() {
        // #10: zwei gleichzeitige Vorgänge mit gleichen Datei-IDs.
        let dst = tmpdir("inter");
        let mut rx = Receiver::new(dst.clone());
        rx.handle(&start(1, 0, "eins.txt", 2));
        rx.handle(&start(2, 0, "zwei.txt", 2));
        rx.handle(&chunk(2, 0, b"zw"));
        rx.handle(&chunk(1, 0, b"ei"));
        rx.handle(&end(2, 0));
        match rx.handle(&batch_end(2, 1)) {
            Some(ReceiveEvent::Done { files, .. }) => {
                assert_eq!(files, vec![dst.join("zwei.txt")])
            }
            other => panic!("{other:?}"),
        }
        // Ein abgebrochener Vorgang setzt den anderen nicht zurück.
        rx.handle(&start(3, 0, "drei.txt", 5));
        assert!(failed(rx.handle(&end(3, 0))));
        rx.handle(&end(1, 0));
        match rx.handle(&batch_end(1, 1)) {
            Some(ReceiveEvent::Done { files, .. }) => {
                assert_eq!(files, vec![dst.join("eins.txt")])
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read(dst.join("eins.txt")).unwrap(), b"ei");
    }

    #[test]
    fn concurrent_sends_through_one_outbox() {
        let src = tmpdir("conc-src");
        fs::write(src.join("a.txt"), vec![b'a'; 50_000]).unwrap();
        fs::write(src.join("b.txt"), vec![b'b'; 50_000]).unwrap();
        let ca = collect(&[src.join("a.txt")]).unwrap();
        let cb = collect(&[src.join("b.txt")]).unwrap();
        let dst = tmpdir("conc-dst");
        let mut rx = Receiver::new(dst.clone());
        let outbox = Outbox::new();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| send(&outbox, &ca, 1, false, &cancel, |_, _| {}).unwrap());
            s.spawn(|| send(&outbox, &cb, 2, false, &cancel, |_, _| {}).unwrap());
            let mut done = 0;
            while done < 2 {
                let m = outbox.pop(Duration::from_secs(5)).expect("Nachricht");
                match rx.handle(&m) {
                    Some(ReceiveEvent::Done { .. }) => done += 1,
                    Some(ReceiveEvent::Failed { error, .. }) => panic!("{error}"),
                    _ => {}
                }
            }
        });
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), vec![b'a'; 50_000]);
        assert_eq!(fs::read(dst.join("b.txt")).unwrap(), vec![b'b'; 50_000]);
    }

    #[test]
    fn limits_are_enforced_before_writing() {
        // #12
        let dst = tmpdir("limits");
        let mut rx = Receiver::new(dst.clone());
        rx.set_limits(Limits {
            max_file_bytes: 100,
            max_total_bytes: 150,
            max_files: 2,
            keep_free_bytes: 0,
        });
        assert!(failed(rx.handle(&start(1, 0, "gross", 101))));
        rx.handle(&start(2, 0, "a", 100));
        assert!(failed(rx.handle(&start(2, 1, "b", 51))));
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));
        rx.handle(&start(3, 0, "a", 0));
        rx.handle(&start(3, 1, "b", 0));
        assert!(failed(rx.handle(&start(3, 2, "c", 0))));
        // Überlauf der Summe.
        rx.set_limits(Limits {
            max_file_bytes: u64::MAX,
            max_total_bytes: u64::MAX,
            max_files: 10,
            keep_free_bytes: 0,
        });
        rx.handle(&start(4, 0, "a", u64::MAX));
        assert!(failed(rx.handle(&start(4, 1, "b", 1))));
        // Zu wenig Platz.
        rx.set_free_space_probe(|_| Some(10));
        assert!(failed(rx.handle(&start(5, 0, "a", 11))));
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_receive_dir_are_not_followed() {
        // #14
        use std::os::unix::fs::symlink;
        let dst = tmpdir("sym");
        let outside = tmpdir("sym-outside");
        symlink(&outside, dst.join("Ordner")).unwrap();
        let mut rx = Receiver::new(dst.clone());
        // Ordner existiert als Symlink → eindeutiger Name, kein Durchschreiben.
        rx.handle(&start(1, 0, "Ordner/x.txt", 1));
        rx.handle(&chunk(1, 0, b"x"));
        rx.handle(&end(1, 0));
        assert!(matches!(
            rx.handle(&batch_end(1, 1)),
            Some(ReceiveEvent::Done { .. })
        ));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        assert_eq!(fs::read(dst.join("Ordner (2)/x.txt")).unwrap(), b"x");

        // Kaputter Symlink als Ziel wird nicht überschrieben.
        symlink(outside.join("ziel"), dst.join("b.txt")).unwrap();
        rx.handle(&start(2, 0, "b.txt", 1));
        rx.handle(&chunk(2, 0, b"b"));
        rx.handle(&end(2, 0));
        rx.handle(&batch_end(2, 1));
        assert!(!outside.join("ziel").exists());
        assert_eq!(fs::read(dst.join("b (2).txt")).unwrap(), b"b");

        // Symlink tiefer im Pfad, der erst nach dem Anlegen auftaucht.
        rx.handle(&start(3, 0, "Neu/a.txt", 1));
        let real = dst.join("Neu");
        fs::create_dir_all(&real).unwrap();
        symlink(&outside, real.join("tief")).unwrap();
        assert!(failed(rx.handle(&start(3, 1, "Neu/tief/b.txt", 1))));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn collect_reports_skipped_and_limit() {
        // #15
        let src = tmpdir("skip");
        fs::write(src.join("a.txt"), b"a").unwrap();
        let c = collect(&[src.join("a.txt"), src.join("fehlt.txt")]).unwrap();
        assert_eq!(c.items.len(), 1);
        assert_eq!(c.skipped.len(), 1);
        assert_eq!(c.skipped[0].0, src.join("fehlt.txt"));

        let many = tmpdir("many");
        for i in 0..=MAX_FILES {
            File::create(many.join(format!("{i}"))).unwrap();
        }
        let err = collect(std::slice::from_ref(&many)).unwrap_err();
        assert!(err.to_string().contains("zu viele"));
        let _ = fs::remove_dir_all(many);
    }

    #[cfg(unix)]
    #[test]
    fn collect_reports_symlinks() {
        let src = tmpdir("skip-sym");
        std::os::unix::fs::symlink("/etc/hosts", src.join("link")).unwrap();
        let c = collect(std::slice::from_ref(&src)).unwrap();
        assert!(c.items.is_empty());
        assert_eq!(c.skipped.len(), 1);
    }

    #[test]
    fn discard_removes_partial_transfer_silently() {
        let dst = tmpdir("discard");
        let mut rx = Receiver::new(dst.clone());
        rx.handle(&start(4, 0, "a.txt", 3));
        rx.handle(&chunk(4, 0, b"ab"));
        rx.discard(4);
        assert_eq!(rx.handle(&chunk(4, 0, b"c")), None);
        assert_eq!(rx.handle(&end(4, 0)), None);
        assert!(tree(&dst).is_empty(), "{:?}", tree(&dst));
    }

    #[test]
    fn cancel_stops_sender() {
        let src = tmpdir("cancel");
        fs::write(src.join("a.txt"), b"a").unwrap();
        let c = collect(&[src.join("a.txt")]).unwrap();
        let outbox = Outbox::new();
        let cancel = AtomicBool::new(true);
        let e = send(&outbox, &c, 1, false, &cancel, |_, _| {}).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
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
