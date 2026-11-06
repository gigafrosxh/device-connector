//! Kleiner WebDAV-Server auf 127.0.0.1: macht die Laufwerke des anderen
//! Rechners für Finder und Explorer als Netzlaufwerk sichtbar.
//!
//! Jede Dateioperation wird als [`FsOp`] an ein [`Backend`] gegeben (im
//! Betrieb: über die Verbindung an den anderen Rechner). Adressen haben die
//! Form `/<token>/<name>/<Laufwerk>/<Pfad>`: Das zufällige Token schützt vor
//! anderen Programmen und Webseiten, `<name>` ist nur der Anzeigename beim
//! Einbinden und wird ignoriert. Ein Browser bekommt eine einfache
//! Ordnerliste (Rückfallebene, falls das Einbinden nicht klappt).

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dc_core::remotefs::{FsData, FsEntry, FsError, FsErrorKind, FsOp, FsResult, MAX_IO};

/// Wartezeit auf eine einzelne Antwort des anderen Rechners.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// So viele Lese-/Schreibblöcke dürfen gleichzeitig unterwegs sein.
const WINDOW: usize = 8;
const MAX_HEADER: usize = 64 * 1024;
const MAX_XML_BODY: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub trait Backend: Send + Sync + 'static {
    /// Anfrage abschicken; die Antwort kommt über den Empfänger.
    fn submit(&self, op: FsOp) -> Receiver<FsResult>;
}

fn wait(rx: Receiver<FsResult>) -> FsResult {
    rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| {
        Err(FsError::new(
            FsErrorKind::Unavailable,
            "keine Antwort vom anderen Rechner",
        ))
    })
}

fn call(b: &dyn Backend, op: FsOp) -> FsResult {
    wait(b.submit(op))
}

pub struct DavServer {
    pub port: u16,
    pub token: String,
}

impl DavServer {
    /// Adresse zum Einbinden; `label` wird zum Namen des Laufwerks.
    pub fn url(&self, label: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}/{}/",
            self.port,
            self.token,
            encode_segment(label)
        )
    }
}

/// Server starten. `preferred_port` wird versucht (gleiche Adresse über
/// Neustarts hinweg), sonst ein freier.
pub fn start(backend: Arc<dyn Backend>, preferred_port: u16) -> io::Result<DavServer> {
    let listener = TcpListener::bind(("127.0.0.1", preferred_port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
    let port = listener.local_addr()?.port();
    let token = format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    );
    let ctx = Arc::new(Ctx {
        backend,
        token: token.clone(),
        port,
        active: AtomicUsize::new(0),
    });
    thread::Builder::new()
        .name("dc-dav".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if ctx.active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                ctx.active.fetch_add(1, Ordering::SeqCst);
                let ctx = ctx.clone();
                let _ = thread::Builder::new()
                    .name("dc-dav-conn".into())
                    .spawn(move || {
                        if let Err(e) = connection(&ctx, stream) {
                            log::debug!("WebDAV-Verbindung: {e}");
                        }
                        ctx.active.fetch_sub(1, Ordering::SeqCst);
                    });
            }
        })?;
    log::info!("Laufwerke des anderen Rechners: WebDAV auf 127.0.0.1:{port}");
    Ok(DavServer { port, token })
}

struct Ctx {
    backend: Arc<dyn Backend>,
    token: String,
    port: u16,
    active: AtomicUsize,
}

// ---------------------------------------------------------------- HTTP

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    }
}

/// `None`: Verbindung sauber zu Ende.
fn read_head(r: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let mut total = 0usize;
    let mut line = String::new();
    // Leere Zeilen vor der Anfrage überspringen (RFC 9112).
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        total += n;
        if !line.trim().is_empty() {
            break;
        }
        if total > MAX_HEADER {
            return Err(io::Error::other("Kopf zu groß"));
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        total += n;
        if total > MAX_HEADER {
            return Err(io::Error::other("Kopf zu groß"));
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Some(Request {
        method,
        target,
        headers,
    }))
}

enum BodyKind {
    Length(u64),
    Chunked,
}

/// Liest den Rumpf einer Anfrage (feste Länge oder `chunked`).
struct Body<'a> {
    r: &'a mut BufReader<TcpStream>,
    kind: BodyKind,
    /// Rest des aktuellen Stücks bei `chunked`.
    chunk_left: u64,
    done: bool,
}

impl<'a> Body<'a> {
    fn new(r: &'a mut BufReader<TcpStream>, req: &Request) -> io::Result<Body<'a>> {
        let chunked = req
            .header("Transfer-Encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let kind = if chunked {
            BodyKind::Chunked
        } else {
            let len = match req.header("Content-Length") {
                Some(v) => v
                    .parse::<u64>()
                    .map_err(|_| io::Error::other("ungültige Content-Length"))?,
                None => 0,
            };
            BodyKind::Length(len)
        };
        let done = matches!(kind, BodyKind::Length(0));
        Ok(Body {
            r,
            kind,
            chunk_left: 0,
            done,
        })
    }

    fn read_line(&mut self) -> io::Result<String> {
        let mut s = String::new();
        let n = self.r.by_ref().take(4096).read_line(&mut s)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(s)
    }

    /// Alles lesen, höchstens `max` Bytes.
    fn read_all(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() + n > max {
                return Err(io::Error::other("Rumpf zu groß"));
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Bis `buf` voll ist oder der Rumpf zu Ende.
    fn fill(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut got = 0;
        while got < buf.len() {
            let n = self.read(&mut buf[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        match self.kind {
            BodyKind::Length(ref mut left) => {
                let want = (*left).min(buf.len() as u64) as usize;
                let n = self.r.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                *left -= n as u64;
                if *left == 0 {
                    self.done = true;
                }
                Ok(n)
            }
            BodyKind::Chunked => {
                if self.chunk_left == 0 {
                    let line = self.read_line()?;
                    let size = line.trim().split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size, 16)
                        .map_err(|_| io::Error::other("ungültige Stückgröße"))?;
                    if size == 0 {
                        // Anhänge bis zur Leerzeile überspringen.
                        loop {
                            let l = self.read_line()?;
                            if l.trim().is_empty() {
                                break;
                            }
                        }
                        self.done = true;
                        return Ok(0);
                    }
                    self.chunk_left = size;
                }
                let want = self.chunk_left.min(buf.len() as u64) as usize;
                let n = self.r.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                self.chunk_left -= n as u64;
                if self.chunk_left == 0 {
                    self.read_line()?; // CRLF nach dem Stück
                }
                Ok(n)
            }
        }
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, k: &str, v: impl Into<String>) -> Response {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    fn body(mut self, ctype: &str, body: impl Into<Vec<u8>>) -> Response {
        self.body = body.into();
        self.header("Content-Type", ctype)
    }

    fn text(status: u16, msg: &str) -> Response {
        Response::new(status).body("text/plain; charset=utf-8", format!("{msg}\n"))
    }

    fn error(e: &FsError) -> Response {
        Response::text(status_for(e.kind), &e.message)
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        423 => "Locked",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "",
    }
}

fn status_for(kind: FsErrorKind) -> u16 {
    match kind {
        FsErrorKind::NotFound => 404,
        FsErrorKind::Denied | FsErrorKind::Disabled => 403,
        FsErrorKind::Exists => 405,
        FsErrorKind::NotEmpty => 409,
        FsErrorKind::Invalid => 400,
        FsErrorKind::Unavailable => 503,
        FsErrorKind::Other => 500,
    }
}

fn write_head(
    w: &mut dyn Write,
    status: u16,
    headers: &[(String, String)],
    len: u64,
    keep: bool,
) -> io::Result<()> {
    let mut s = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str(&format!("Content-Length: {len}\r\n"));
    s.push_str("Server: DeviceConnector\r\n");
    s.push_str(if keep {
        "Connection: keep-alive\r\n\r\n"
    } else {
        "Connection: close\r\n\r\n"
    });
    w.write_all(s.as_bytes())
}

fn send(w: &mut dyn Write, r: &Response, head_only: bool, keep: bool) -> io::Result<()> {
    write_head(w, r.status, &r.headers, r.body.len() as u64, keep)?;
    if !head_only {
        w.write_all(&r.body)?;
    }
    w.flush()
}

fn connection(ctx: &Ctx, stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let _ = stream.set_nodelay(true);
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    loop {
        let Some(req) = read_head(&mut reader)? else {
            return Ok(());
        };
        let wants_close = req
            .header("Connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        if req
            .header("Expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
        {
            out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        let mut body = Body::new(&mut reader, &req)?;
        let keep = handle(ctx, &req, &mut body, &mut out)? && !wants_close;
        // Nicht gelesenen Rest verwerfen, damit die nächste Anfrage passt.
        let rest_ok = body.done || {
            let drained = io::copy(&mut body.by_ref().take(1 << 20), &mut io::sink()).is_ok();
            drained && body.done
        };
        if !keep || !rest_ok {
            let _ = out.shutdown(Shutdown::Both);
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------- Pfade

/// Zeichen, die in einem Pfadteil unverändert bleiben dürfen.
fn keep_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=@".contains(&b)
}

pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if keep_char(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Was eine Adresse bezeichnet.
#[derive(Debug, PartialEq)]
enum Target {
    /// `/` oder `/<token>`: Elternordner der Freigabe (nur fürs Einbinden).
    Ancestor,
    /// Innerhalb der Freigabe: `label` (roh, wie angefragt) und virtueller Pfad.
    Share {
        label: String,
        vpath: Vec<String>,
    },
    Forbidden,
}

fn parse_target(token: &str, target: &str) -> Option<Target> {
    // Absolute Form (http://host/pfad) zulassen.
    let path = match target.find("://") {
        Some(i) => {
            let rest = &target[i + 3..];
            &rest[rest.find('/').unwrap_or(rest.len())..]
        }
        None => target,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut segs = Vec::new();
    for raw in path.split('/').filter(|s| !s.is_empty()) {
        let s = decode(raw)?;
        if s == "." || s == ".." || s.contains('/') || s.contains('\\') || s.contains('\0') {
            return None;
        }
        segs.push(s);
    }
    Some(match segs.len() {
        0 => Target::Ancestor,
        _ if segs[0] != token => Target::Forbidden,
        1 => Target::Ancestor,
        _ => {
            let label = segs[1].clone();
            Target::Share {
                label,
                vpath: segs.split_off(2),
            }
        }
    })
}

fn href(token: &str, label: &str, vpath: &[String], dir: bool) -> String {
    let mut s = format!("/{token}/{}/", encode_segment(label));
    let parts: Vec<String> = vpath.iter().map(|p| encode_segment(p)).collect();
    s.push_str(&parts.join("/"));
    if dir && !vpath.is_empty() {
        s.push('/');
    }
    s
}

fn host_ok(req: &Request, port: u16) -> bool {
    let Some(h) = req.header("Host") else {
        return true;
    };
    let h = h.to_ascii_lowercase();
    let host = match h.rsplit_once(':') {
        Some((host, p)) if !h.ends_with(']') => {
            if p != port.to_string() {
                return false;
            }
            host.to_string()
        }
        _ => h,
    };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

// ---------------------------------------------------------------- Zeit

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split_time(secs: i64) -> (i64, u32, u32, i64, i64, i64, usize) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    // 1970-01-01 war ein Donnerstag.
    let wd = (days + 4).rem_euclid(7) as usize;
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60, wd)
}

/// RFC 1123, z. B. `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss, wd) = split_time(secs);
    format!(
        "{}, {d:02} {} {y:04} {hh:02}:{mm:02}:{ss:02} GMT",
        DAYS[wd],
        MONTHS[m as usize - 1]
    )
}

fn iso_date(secs: i64) -> String {
    let (y, m, d, hh, mm, ss, _) = split_time(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

// ---------------------------------------------------------------- XML

fn xml_esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            // In XML 1.0 nicht erlaubte Steuerzeichen.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => o.push('\u{fffd}'),
            c => o.push(c),
        }
    }
    o
}

const MULTI_OPEN: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:Z=\"urn:schemas-microsoft-com:\">\n";
const MULTI_CLOSE: &str = "</D:multistatus>\n";

fn prop_response(href: &str, e: &FsEntry) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "<D:displayname>{}</D:displayname>",
        xml_esc(&e.name)
    ));
    if e.dir {
        p.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
    } else {
        p.push_str("<D:resourcetype/>");
        p.push_str(&format!(
            "<D:getcontentlength>{}</D:getcontentlength>",
            e.size
        ));
        p.push_str("<D:getcontenttype>application/octet-stream</D:getcontenttype>");
    }
    let modified = if e.modified > 0 { e.modified } else { 0 };
    let created = if e.created > 0 { e.created } else { modified };
    p.push_str(&format!(
        "<D:getlastmodified>{}</D:getlastmodified><D:creationdate>{}</D:creationdate>",
        http_date(modified),
        iso_date(created)
    ));
    p.push_str(&format!(
        "<D:getetag>\"{:x}-{:x}\"</D:getetag>",
        e.size, e.modified
    ));
    p.push_str("<D:supportedlock><D:lockentry><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockentry></D:supportedlock><D:lockdiscovery/>");
    if let (Some(total), Some(free)) = (e.total, e.free) {
        p.push_str(&format!(
            "<D:quota-available-bytes>{free}</D:quota-available-bytes><D:quota-used-bytes>{}</D:quota-used-bytes>",
            total.saturating_sub(free)
        ));
    }
    let mut attrs: u32 = if e.dir { 0x10 } else { 0x20 };
    if e.hidden {
        attrs |= 0x02;
    }
    if e.readonly && !e.dir {
        attrs |= 0x01;
    }
    p.push_str(&format!(
        "<Z:Win32FileAttributes>{attrs:08X}</Z:Win32FileAttributes>"
    ));
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{p}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n",
        xml_esc(href)
    )
}

/// Lokale Namen der Eigenschaften in einem PROPPATCH (ohne XML-Bibliothek).
fn proppatch_names(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut depth_in_prop = 0;
    let mut rest = body;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        rest = &rest[end.min(rest.len())..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == "prop" {
            if closing {
                depth_in_prop = 0;
            } else if !self_closing {
                depth_in_prop = 1;
            }
            continue;
        }
        if depth_in_prop == 1 && !closing {
            let ns = if local.starts_with("Win32") {
                "urn:schemas-microsoft-com:"
            } else {
                "DAV:"
            };
            out.push((local.to_string(), ns.to_string()));
            if !self_closing {
                depth_in_prop = 2;
            }
        } else if depth_in_prop == 2 && closing {
            depth_in_prop = 1;
        }
    }
    out
}

fn lock_owner(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let Some(open) = lower.find("owner>") else {
        return String::new();
    };
    let start = open + "owner>".len();
    let Some(close_rel) = lower[start..].rfind("</") else {
        return String::new();
    };
    // Letztes schließendes owner-Tag suchen.
    let close = lower[start..]
        .match_indices("</")
        .map(|(i, _)| start + i)
        .find(|&i| {
            lower[i..]
                .split('>')
                .next()
                .is_some_and(|t| t.ends_with("owner"))
        })
        .unwrap_or(start + close_rel);
    body[start..close].to_string()
}

// ---------------------------------------------------------------- Methoden

/// Gibt zurück, ob die Verbindung offen bleiben darf.
fn handle(ctx: &Ctx, req: &Request, body: &mut Body, out: &mut TcpStream) -> io::Result<bool> {
    let head_only = req.method == "HEAD";
    if !host_ok(req, ctx.port) {
        send(out, &Response::text(403, "Falscher Host"), false, false)?;
        return Ok(false);
    }
    let Some(target) = parse_target(&ctx.token, &req.target) else {
        send(
            out,
            &Response::text(400, "Ungültige Adresse"),
            head_only,
            true,
        )?;
        return Ok(true);
    };
    let dav_headers = |r: Response| {
        r.header("DAV", "1, 2")
            .header("MS-Author-Via", "DAV")
            .header(
            "Allow",
            "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, COPY, MOVE, PROPFIND, PROPPATCH, LOCK, UNLOCK",
        )
    };
    if req.method == "OPTIONS" {
        send(out, &dav_headers(Response::new(200)), false, true)?;
        return Ok(true);
    }
    let b = &*ctx.backend;
    let resp = match target {
        Target::Forbidden => Response::text(404, "Nicht gefunden"),
        Target::Ancestor => match req.method.as_str() {
            "PROPFIND" => {
                let _ = body.read_all(MAX_XML_BODY);
                let path = req.target.split('?').next().unwrap_or("/");
                let href = if path.ends_with('/') {
                    path.to_string()
                } else {
                    format!("{path}/")
                };
                let mut x = String::from(MULTI_OPEN);
                x.push_str(&prop_response(
                    &href,
                    &FsEntry {
                        name: String::new(),
                        dir: true,
                        size: 0,
                        modified: 0,
                        created: 0,
                        readonly: true,
                        hidden: false,
                        total: None,
                        free: None,
                    },
                ));
                x.push_str(MULTI_CLOSE);
                Response::new(207).body("application/xml; charset=utf-8", x)
            }
            "GET" | "HEAD" => Response::text(404, "Device Connector"),
            _ => Response::text(403, "Nur lesen"),
        },
        Target::Share { label, vpath } => {
            let vp = vpath.join("/");
            match req.method.as_str() {
                "PROPFIND" => propfind(b, req, body, &ctx.token, &label, &vpath),
                "GET" | "HEAD" => {
                    return get(b, req, out, &ctx.token, &label, &vpath, &vp, head_only);
                }
                "PUT" => put(b, body, &vp)?,
                "DELETE" => match call(b, FsOp::Remove { path: vp }) {
                    Ok(_) => Response::new(204),
                    Err(e) => Response::error(&e),
                },
                "MKCOL" => {
                    let extra = body.read_all(16)?;
                    if !extra.is_empty() {
                        Response::text(415, "MKCOL mit Rumpf")
                    } else {
                        match call(b, FsOp::Mkdir { path: vp }) {
                            Ok(_) => Response::new(201),
                            Err(e) if e.kind == FsErrorKind::NotFound => {
                                Response::text(409, "Elternordner fehlt")
                            }
                            Err(e) => Response::error(&e),
                        }
                    }
                }
                "MOVE" | "COPY" => move_copy(ctx, req, &vp),
                "PROPPATCH" => {
                    let x = body.read_all(MAX_XML_BODY)?;
                    let names = proppatch_names(&String::from_utf8_lossy(&x));
                    let mut props = String::new();
                    for (n, ns) in names {
                        props.push_str(&format!("<{n} xmlns=\"{ns}\"/>"));
                    }
                    let h = href(&ctx.token, &label, &vpath, false);
                    let xml = format!(
                        "{MULTI_OPEN}<D:response><D:href>{}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n{MULTI_CLOSE}",
                        xml_esc(&h)
                    );
                    Response::new(207).body("application/xml; charset=utf-8", xml)
                }
                "LOCK" => lock(b, req, body, &ctx.token, &label, &vpath)?,
                "UNLOCK" => Response::new(204),
                _ => Response::text(501, "Nicht unterstützt"),
            }
        }
    };
    send(out, &resp, head_only, true)?;
    Ok(true)
}

fn propfind(
    b: &dyn Backend,
    req: &Request,
    body: &mut Body,
    token: &str,
    label: &str,
    vpath: &[String],
) -> Response {
    let _ = body.read_all(MAX_XML_BODY);
    let vp = vpath.join("/");
    let depth = req.header("Depth").unwrap_or("infinity");
    let me = match call(b, FsOp::Stat { path: vp.clone() }) {
        Ok(FsData::Entry(e)) => e,
        Ok(_) => return Response::text(500, "Unerwartete Antwort"),
        Err(e) => return Response::error(&e),
    };
    let mut x = String::from(MULTI_OPEN);
    let mut me_named = me.clone();
    if me_named.name.is_empty() {
        me_named.name = label.to_string();
    }
    x.push_str(&prop_response(
        &href(token, label, vpath, me.dir),
        &me_named,
    ));
    // „infinity“ wie 1 behandeln: ganze Platten rekursiv wäre zu teuer.
    if me.dir && depth != "0" {
        match call(b, FsOp::List { path: vp }) {
            Ok(FsData::Entries(list)) => {
                for e in list {
                    let mut p = vpath.to_vec();
                    p.push(e.name.clone());
                    x.push_str(&prop_response(&href(token, label, &p, e.dir), &e));
                }
            }
            Ok(_) => {}
            Err(e) => return Response::error(&e),
        }
    }
    x.push_str(MULTI_CLOSE);
    Response::new(207).body("application/xml; charset=utf-8", x)
}

/// `Range: bytes=a-b` → (Anfang, Ende exklusiv). `None`: ganze Datei.
fn parse_range(h: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = h else { return Ok(None) };
    let Some(spec) = h.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None); // mehrere Bereiche: ganze Datei schicken
    }
    let (a, z) = spec.split_once('-').ok_or(())?;
    let (a, z) = (a.trim(), z.trim());
    let range = if a.is_empty() {
        let n: u64 = z.parse().map_err(|_| ())?;
        (size.saturating_sub(n), size)
    } else {
        let a: u64 = a.parse().map_err(|_| ())?;
        let z = if z.is_empty() {
            size
        } else {
            (z.parse::<u64>().map_err(|_| ())? + 1).min(size)
        };
        (a, z)
    };
    if range.0 >= range.1 {
        return Err(());
    }
    Ok(Some(range))
}

#[allow(clippy::too_many_arguments)]
fn get(
    b: &dyn Backend,
    req: &Request,
    out: &mut TcpStream,
    token: &str,
    label: &str,
    vpath: &[String],
    vp: &str,
    head_only: bool,
) -> io::Result<bool> {
    let me = match call(b, FsOp::Stat { path: vp.into() }) {
        Ok(FsData::Entry(e)) => e,
        Ok(_) => {
            send(out, &Response::text(500, "Unerwartet"), head_only, true)?;
            return Ok(true);
        }
        Err(e) => {
            send(out, &Response::error(&e), head_only, true)?;
            return Ok(true);
        }
    };
    if me.dir {
        let resp = match call(b, FsOp::List { path: vp.into() }) {
            Ok(FsData::Entries(list)) => Response::new(200).body(
                "text/html; charset=utf-8",
                listing_html(token, label, vpath, &list),
            ),
            Ok(_) => Response::text(500, "Unerwartet"),
            Err(e) => Response::error(&e),
        };
        send(out, &resp, head_only, true)?;
        return Ok(true);
    }
    let size = me.size;
    let (status, start, end) = match parse_range(req.header("Range"), size) {
        Ok(None) => (200, 0, size),
        Ok(Some((a, z))) => (206, a, z),
        Err(()) => {
            let r = Response::new(416).header("Content-Range", format!("bytes */{size}"));
            send(out, &r, head_only, true)?;
            return Ok(true);
        }
    };
    let mut headers = vec![
        ("Content-Type".to_string(), content_type(vp).to_string()),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        ("Last-Modified".to_string(), http_date(me.modified)),
        (
            "ETag".to_string(),
            format!("\"{:x}-{:x}\"", me.size, me.modified),
        ),
    ];
    if status == 206 {
        headers.push((
            "Content-Range".to_string(),
            format!("bytes {start}-{}/{size}", end - 1),
        ));
    }
    write_head(out, status, &headers, end - start, true)?;
    if head_only {
        out.flush()?;
        return Ok(true);
    }
    let mut inflight: VecDeque<(u64, Receiver<FsResult>)> = VecDeque::new();
    let mut next = start;
    loop {
        while next < end && inflight.len() < WINDOW {
            let len = (end - next).min(MAX_IO as u64) as u32;
            let rx = b.submit(FsOp::Read {
                path: vp.into(),
                offset: next,
                len,
            });
            inflight.push_back((len as u64, rx));
            next += len as u64;
        }
        let Some((want, rx)) = inflight.pop_front() else {
            break;
        };
        match wait(rx) {
            Ok(FsData::Data(d)) if d.len() as u64 == want => out.write_all(&d)?,
            other => {
                // Länge ist schon gesendet: Verbindung abbrechen, damit der
                // Client die Datei nicht für vollständig hält.
                log::warn!("Lesen von {vp} abgebrochen: {:?}", other.err());
                let _ = out.shutdown(Shutdown::Both);
                return Ok(false);
            }
        }
    }
    out.flush()?;
    Ok(true)
}

fn put(b: &dyn Backend, body: &mut Body, vp: &str) -> io::Result<Response> {
    let existed = match call(b, FsOp::Stat { path: vp.into() }) {
        Ok(FsData::Entry(e)) if e.dir => return Ok(Response::text(405, "Ist ein Ordner")),
        Ok(_) => true,
        Err(e) if e.kind == FsErrorKind::NotFound => false,
        Err(e) => return Ok(Response::error(&e)),
    };
    let mut buf = vec![0u8; MAX_IO as usize];
    let n = body.fill(&mut buf)?;
    // Erster Block legt die Datei an (auch wenn sie leer bleibt).
    let first = call(
        b,
        FsOp::Write {
            path: vp.into(),
            offset: 0,
            data: buf[..n].to_vec(),
            truncate: true,
        },
    );
    if let Err(e) = first {
        return Ok(match e.kind {
            FsErrorKind::NotFound => Response::text(409, "Elternordner fehlt"),
            _ => Response::error(&e),
        });
    }
    let mut offset = n as u64;
    let mut inflight: VecDeque<Receiver<FsResult>> = VecDeque::new();
    let mut failed: Option<FsError> = None;
    loop {
        let n = body.fill(&mut buf)?;
        if n == 0 {
            break;
        }
        if failed.is_none() {
            inflight.push_back(b.submit(FsOp::Write {
                path: vp.into(),
                offset,
                data: buf[..n].to_vec(),
                truncate: false,
            }));
            offset += n as u64;
            if inflight.len() >= WINDOW {
                if let Err(e) = wait(inflight.pop_front().unwrap()) {
                    failed = Some(e);
                }
            }
        }
        // Nach einem Fehler den Rumpf trotzdem zu Ende lesen.
    }
    for rx in inflight {
        if let Err(e) = wait(rx) {
            failed.get_or_insert(e);
        }
    }
    Ok(match failed {
        Some(e) => {
            log::warn!("Schreiben nach {vp} fehlgeschlagen: {e}");
            Response::error(&e)
        }
        None if existed => Response::new(204),
        None => Response::new(201),
    })
}

fn move_copy(ctx: &Ctx, req: &Request, from: &str) -> Response {
    let Some(dest) = req.header("Destination") else {
        return Response::text(400, "Destination fehlt");
    };
    let to = match parse_target(&ctx.token, dest) {
        Some(Target::Share { vpath, .. }) if !vpath.is_empty() => vpath.join("/"),
        _ => return Response::text(502, "Ziel liegt nicht in der Freigabe"),
    };
    let overwrite = !req
        .header("Overwrite")
        .is_some_and(|v| v.eq_ignore_ascii_case("F"));
    let b = &*ctx.backend;
    let existed = call(b, FsOp::Stat { path: to.clone() }).is_ok();
    let op = if req.method == "MOVE" {
        FsOp::Rename {
            from: from.into(),
            to,
            overwrite,
        }
    } else {
        FsOp::Copy {
            from: from.into(),
            to,
            overwrite,
        }
    };
    match call(b, op) {
        Ok(_) if existed => Response::new(204),
        Ok(_) => Response::new(201),
        Err(e) if e.kind == FsErrorKind::Exists => Response::text(412, "Ziel existiert"),
        Err(e) if e.kind == FsErrorKind::NotFound && existed => Response::error(&e),
        Err(e) => Response::error(&e),
    }
}

fn lock(
    b: &dyn Backend,
    req: &Request,
    body: &mut Body,
    token: &str,
    label: &str,
    vpath: &[String],
) -> io::Result<Response> {
    let x = body.read_all(MAX_XML_BODY)?;
    let x = String::from_utf8_lossy(&x).to_string();
    let vp = vpath.join("/");
    let mut status = 200;
    let mut dir = false;
    if !x.trim().is_empty() {
        // Neue Sperre. Auf eine fehlende Datei: leer anlegen (RFC 4918 9.10.4).
        match call(b, FsOp::Stat { path: vp.clone() }) {
            Ok(FsData::Entry(e)) => dir = e.dir,
            Err(e) if e.kind == FsErrorKind::NotFound && !vpath.is_empty() => {
                match call(
                    b,
                    FsOp::Write {
                        path: vp.clone(),
                        offset: 0,
                        data: Vec::new(),
                        truncate: true,
                    },
                ) {
                    Ok(_) => status = 201,
                    Err(e) if e.kind == FsErrorKind::NotFound => {
                        return Ok(Response::text(409, "Elternordner fehlt"))
                    }
                    Err(e) => return Ok(Response::error(&e)),
                }
            }
            Err(e) => return Ok(Response::error(&e)),
            Ok(_) => {}
        }
    }
    // Erneuern: Token aus dem If-Kopf übernehmen.
    let lock_token = req
        .header("If")
        .and_then(|h| {
            let s = h.find("opaquelocktoken:")?;
            let rest = &h[s..];
            Some(rest[..rest.find('>').unwrap_or(rest.len())].to_string())
        })
        .unwrap_or_else(|| {
            format!(
                "opaquelocktoken:{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
                rand::random::<u32>(),
                rand::random::<u16>(),
                rand::random::<u16>() & 0xfff,
                rand::random::<u16>() & 0xfff,
                rand::random::<u64>() & 0xffff_ffff_ffff
            )
        });
    let depth = if req.header("Depth") == Some("0") || !dir {
        "0"
    } else {
        "infinity"
    };
    let h = href(token, label, vpath, dir);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery><D:activelock><D:locktype><D:write/></D:locktype><D:lockscope><D:exclusive/></D:lockscope><D:depth>{depth}</D:depth><D:owner>{}</D:owner><D:timeout>Second-3600</D:timeout><D:locktoken><D:href>{lock_token}</D:href></D:locktoken><D:lockroot><D:href>{}</D:href></D:lockroot></D:activelock></D:lockdiscovery></D:prop>\n",
        lock_owner(&x),
        xml_esc(&h)
    );
    Ok(Response::new(status)
        .header("Lock-Token", format!("<{lock_token}>"))
        .body("application/xml; charset=utf-8", xml))
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" | "md" | "csv" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Ordnerliste für den Browser.
fn listing_html(token: &str, label: &str, vpath: &[String], list: &[FsEntry]) -> String {
    let title = if vpath.is_empty() {
        format!("Laufwerke von {label}")
    } else {
        format!("{label} / {}", vpath.join(" / "))
    };
    let mut rows = String::new();
    if !vpath.is_empty() {
        let up = href(token, label, &vpath[..vpath.len() - 1], true);
        rows.push_str(&format!(
            "<tr><td><a href=\"{}\">⬆ Übergeordneter Ordner</a></td><td></td><td></td></tr>",
            xml_esc(&up)
        ));
    }
    for e in list.iter().filter(|e| !e.hidden) {
        let mut p = vpath.to_vec();
        p.push(e.name.clone());
        let link = href(token, label, &p, e.dir);
        let size = match (e.dir, e.total, e.free) {
            (true, Some(t), Some(f)) => format!("{} frei von {}", human(f), human(t)),
            (true, _, _) => String::new(),
            (false, _, _) => human(e.size),
        };
        let when = if e.modified > 0 && !vpath.is_empty() {
            iso_date(e.modified).replace('T', " ").replace('Z', " UTC")
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<tr><td><a href=\"{}\">{}{}</a></td><td>{size}</td><td>{when}</td></tr>",
            xml_esc(&link),
            if e.dir { "📁 " } else { "📄 " },
            xml_esc(&e.name)
        ));
    }
    format!(
        "<!doctype html><html lang=\"de\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{t}</title><style>body{{font:15px system-ui,sans-serif;margin:24px;color:#222;background:#fff}}@media(prefers-color-scheme:dark){{body{{color:#ddd;background:#1e1e1e}}a{{color:#8ab4f8}}}}table{{border-collapse:collapse;width:100%}}td{{padding:4px 12px 4px 0;white-space:nowrap}}td:first-child{{white-space:normal;width:100%}}</style></head><body><h1>{t}</h1><table>{rows}</table></body></html>",
        t = xml_esc(&title)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_core::remotefs::{self, Volume};
    use std::path::PathBuf;
    use std::sync::mpsc;

    struct Local(Vec<Volume>);

    impl Backend for Local {
        fn submit(&self, op: FsOp) -> Receiver<FsResult> {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(remotefs::serve(&self.0, &op));
            rx
        }
    }

    fn setup(name: &str) -> (PathBuf, DavServer) {
        let d = std::env::temp_dir().join(format!("dc-dav-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("vol/Ordner")).unwrap();
        std::fs::write(d.join("vol/Grüße.txt"), b"Hallo vom Mac").unwrap();
        let backend = Arc::new(Local(vec![Volume {
            name: "Macintosh HD".into(),
            root: d.join("vol"),
            total: Some(1000),
            free: Some(250),
        }]));
        let srv = start(backend, 0).unwrap();
        (d, srv)
    }

    fn http(srv: &DavServer, req: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        if out.starts_with(b"HTTP/1.1 100 Continue\r\n\r\n") {
            out.drain(..25);
        }
        let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..split]).to_string();
        let status = head[9..12].parse().unwrap();
        (status, head, out[split + 4..].to_vec())
    }

    fn base(srv: &DavServer) -> String {
        format!("/{}/Mac", srv.token)
    }

    #[test]
    fn dates_are_formatted() {
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(iso_date(0), "1970-01-01T00:00:00Z");
        assert_eq!(http_date(1_709_164_800), "Thu, 29 Feb 2024 00:00:00 GMT");
    }

    #[test]
    fn targets_need_token() {
        assert_eq!(parse_target("t", "/"), Some(Target::Ancestor));
        assert_eq!(parse_target("t", "/t"), Some(Target::Ancestor));
        assert_eq!(parse_target("t", "/x/Mac/C"), Some(Target::Forbidden));
        assert_eq!(
            parse_target("t", "http://127.0.0.1:5/t/Mac/Win%20(C)/a%C3%BC?x=1"),
            Some(Target::Share {
                label: "Mac".into(),
                vpath: vec!["Win (C)".into(), "aü".into()]
            })
        );
        assert_eq!(parse_target("t", "/t/Mac/C/%2e%2e/x"), None);
        assert_eq!(parse_target("t", "/t/Mac/C/a%2Fb"), None);
    }

    #[test]
    fn propfind_lists_volumes_and_files() {
        let (_d, srv) = setup("propfind");
        let b = base(&srv);
        let (st, _, body) = http(
            &srv,
            &format!("PROPFIND {b}/ HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nDepth: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", srv.port),
            b"",
        );
        let x = String::from_utf8(body).unwrap();
        assert_eq!(st, 207, "{x}");
        assert!(x.contains("Macintosh%20HD/</D:href>"), "{x}");
        assert!(x.contains("<D:quota-available-bytes>250<"), "{x}");
        let (st, _, body) = http(
            &srv,
            &format!(
                "PROPFIND {b}/Macintosh%20HD HTTP/1.1\r\nDepth: 1\r\nConnection: close\r\n\r\n"
            ),
            b"",
        );
        let x = String::from_utf8(body).unwrap();
        assert_eq!(st, 207);
        assert!(x.contains("Gr%C3%BC%C3%9Fe.txt</D:href>"), "{x}");
        assert!(x.contains("<D:getcontentlength>13<"), "{x}");
        assert!(x.contains("Ordner/</D:href>"), "{x}");
    }

    #[test]
    fn rejects_wrong_token_and_host() {
        let (_d, srv) = setup("token");
        let (st, _, _) = http(
            &srv,
            "PROPFIND /falsch/Mac/ HTTP/1.1\r\nDepth: 1\r\nConnection: close\r\n\r\n",
            b"",
        );
        assert_eq!(st, 404);
        let (st, _, _) = http(
            &srv,
            &format!(
                "GET {}/ HTTP/1.1\r\nHost: boese.example:{}\r\nConnection: close\r\n\r\n",
                base(&srv),
                srv.port
            ),
            b"",
        );
        assert_eq!(st, 403);
    }

    #[test]
    fn put_get_range_move_delete() {
        let (d, srv) = setup("rw");
        let b = base(&srv);
        // Größer als ein Block und chunked, wie vom macOS-Finder.
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut chunked = Vec::new();
        for c in data.chunks(7000) {
            chunked.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            chunked.extend_from_slice(c);
            chunked.extend_from_slice(b"\r\n");
        }
        chunked.extend_from_slice(b"0\r\n\r\n");
        let (st, _, _) = http(
            &srv,
            &format!("PUT {b}/Macintosh%20HD/Ordner/gro%C3%9F.bin HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n"),
            &chunked,
        );
        assert_eq!(st, 201);
        assert_eq!(std::fs::read(d.join("vol/Ordner/groß.bin")).unwrap(), data);

        let (st, head, body) = http(
            &srv,
            &format!(
                "GET {b}/Macintosh%20HD/Ordner/gro%C3%9F.bin HTTP/1.1\r\nConnection: close\r\n\r\n"
            ),
            b"",
        );
        assert_eq!(st, 200, "{head}");
        assert_eq!(body, data);
        let (st, _, body) = http(
            &srv,
            &format!("GET {b}/Macintosh%20HD/Ordner/gro%C3%9F.bin HTTP/1.1\r\nRange: bytes=99990-\r\nConnection: close\r\n\r\n"),
            b"",
        );
        assert_eq!(st, 206);
        assert_eq!(body, &data[99_990..]);

        let (st, _, _) = http(
            &srv,
            &format!("MOVE {b}/Macintosh%20HD/Ordner/gro%C3%9F.bin HTTP/1.1\r\nDestination: http://127.0.0.1:{}{b}/Macintosh%20HD/neu.bin\r\nConnection: close\r\n\r\n", srv.port),
            b"",
        );
        assert_eq!(st, 201);
        assert!(d.join("vol/neu.bin").is_file());
        let (st, _, _) = http(
            &srv,
            &format!("MKCOL {b}/Macintosh%20HD/Leer HTTP/1.1\r\nConnection: close\r\n\r\n"),
            b"",
        );
        assert_eq!(st, 201);
        let (st, _, _) = http(
            &srv,
            &format!("DELETE {b}/Macintosh%20HD/neu.bin HTTP/1.1\r\nConnection: close\r\n\r\n"),
            b"",
        );
        assert_eq!(st, 204);
        assert!(!d.join("vol/neu.bin").exists());
    }

    #[test]
    fn keep_alive_serves_several_requests() {
        let (_d, srv) = setup("keep");
        let b = base(&srv);
        let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        let reqs = format!(
            "OPTIONS / HTTP/1.1\r\n\r\nPUT {b}/Macintosh%20HD/a.txt HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET {b}/Macintosh%20HD/a.txt HTTP/1.1\r\nConnection: close\r\n\r\n"
        );
        s.write_all(reqs.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("DAV: 1, 2"));
        assert!(out.contains("HTTP/1.1 201"));
        assert!(out.ends_with("abc"), "{out}");
    }

    #[test]
    fn lock_creates_and_returns_token() {
        let (d, srv) = setup("lock");
        let b = base(&srv);
        let body = "<?xml version=\"1.0\"?><D:lockinfo xmlns:D=\"DAV:\"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner><D:href>ich</D:href></D:owner></D:lockinfo>";
        let (st, head, x) = http(
            &srv,
            &format!(
                "LOCK {b}/Macintosh%20HD/neu.txt HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ),
            body.as_bytes(),
        );
        assert_eq!(st, 201);
        assert!(head.contains("Lock-Token: <opaquelocktoken:"), "{head}");
        let x = String::from_utf8(x).unwrap();
        assert!(x.contains("<D:owner><D:href>ich</D:href></D:owner>"), "{x}");
        assert!(d.join("vol/neu.txt").is_file());
    }

    #[test]
    fn proppatch_echoes_properties() {
        let names = proppatch_names("<?xml version=\"1.0\"?><D:propertyupdate xmlns:D=\"DAV:\" xmlns:Z=\"urn:schemas-microsoft-com:\"><D:set><D:prop><Z:Win32LastModifiedTime>Thu</Z:Win32LastModifiedTime><Z:Win32FileAttributes>00000020</Z:Win32FileAttributes></D:prop></D:set></D:propertyupdate>");
        let n: Vec<_> = names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(n, vec!["Win32LastModifiedTime", "Win32FileAttributes"]);
    }

    #[test]
    fn browser_listing_escapes_names() {
        let html = listing_html(
            "t",
            "Mac",
            &[],
            &[FsEntry {
                name: "<b>".into(),
                dir: true,
                size: 0,
                modified: 0,
                created: 0,
                readonly: false,
                hidden: false,
                total: Some(2048),
                free: Some(1024),
            }],
        );
        assert!(html.contains("&lt;b&gt;"));
        assert!(html.contains("/t/Mac/%3Cb%3E/"));
        assert!(html.contains("1.0 KB frei von 2.0 KB"));
    }
}
