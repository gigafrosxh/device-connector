//! Anordnungsfenster (#36): Die Bildschirme beider Rechner wie in den
//! Systemeinstellungen nebeneinander ziehen.
//!
//! Ohne eigenes GUI-Toolkit: Die App startet einen winzigen HTTP-Server auf
//! 127.0.0.1 (zufälliger Port, zufälliges Token im Pfad) und öffnet die Seite
//! im Standardbrowser. Die Seite schickt die gewählte Anordnung zurück.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use dc_core::{Align, Side};
use serde::{Deserialize, Serialize};

/// So lange bleibt die Seite benutzbar.
const LIFETIME: Duration = Duration::from_secs(15 * 60);
/// Größte Anfrage, die wir lesen.
const MAX_REQUEST: usize = 16 * 1024;

/// Jedes neue Fenster beendet das vorige.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Was die Seite anzeigt.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub local_name: String,
    pub peer_name: String,
    /// Bildschirmgrößen in Pixeln (Breite, Höhe).
    pub local: (f64, f64),
    pub peer: (f64, f64),
    /// Größe des anderen Bildschirms ist geschätzt (nicht verbunden).
    pub peer_guessed: bool,
    /// Auf welcher Seite der andere Rechner steht.
    pub side: Side,
    /// Ganze Höhe auf ganze Höhe statt Pixel 1:1.
    pub stretch: bool,
    /// Oberkante des anderen Bildschirms relativ zu unserer, in Pixeln.
    pub offset: i32,
}

impl Snapshot {
    pub fn new(
        local_name: String,
        peer_name: String,
        local: (f64, f64),
        peer: Option<(f64, f64)>,
        side: Side,
        align: Align,
    ) -> Snapshot {
        let peer_guessed = peer.is_none();
        let peer = peer.unwrap_or((1920.0, 1080.0));
        Snapshot {
            local_name,
            peer_name,
            local,
            peer,
            peer_guessed,
            side,
            stretch: align == Align::Stretch,
            offset: align.shift(local.1, peer.1).round() as i32,
        }
    }
}

/// Was die Seite zurückschickt.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Choice {
    pub side: Side,
    pub stretch: bool,
    pub offset: i32,
}

impl Choice {
    /// Als Höhenabgleich: Offsets, die genau Ober-/Unterkante oder Mitte
    /// treffen, werden zu den benannten Varianten (die bleiben bei
    /// geänderter Auflösung richtig).
    pub fn align(&self, local_h: f64, peer_h: f64) -> Align {
        if self.stretch {
            return Align::Stretch;
        }
        for a in [Align::Top, Align::Center, Align::Bottom] {
            if (a.shift(local_h, peer_h) - self.offset as f64).abs() < 0.5 {
                return a;
            }
        }
        Align::Offset(self.offset)
    }
}

/// Seite starten. Gibt die Adresse zurück, die geöffnet werden soll.
pub fn start(snapshot: Snapshot, on_save: Box<dyn Fn(Choice) + Send>) -> std::io::Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let token = format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    );
    let gen = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let url = format!("http://127.0.0.1:{port}/{token}");
    let page = page(&snapshot);
    thread::Builder::new()
        .name("dc-arrange".into())
        .spawn(move || {
            let started = Instant::now();
            while started.elapsed() < LIFETIME && GENERATION.load(Ordering::SeqCst) == gen {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(e) = serve(stream, &token, &page, &*on_save) {
                            log::debug!("Anordnungsfenster: {e}");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        log::warn!("Anordnungsfenster: {e}");
                        break;
                    }
                }
            }
        })?;
    Ok(url)
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let head_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_REQUEST {
            return Err(std::io::Error::other("Anfrage zu groß"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let len = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_REQUEST {
        return Err(std::io::Error::other("Anfrage zu groß"));
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Request { method, path, body })
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn serve(
    mut stream: TcpStream,
    token: &str,
    page: &str,
    on_save: &dyn Fn(Choice),
) -> std::io::Result<()> {
    let req = read_request(&mut stream)?;
    let base = format!("/{token}");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", p) if p == base => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            page.as_bytes(),
        ),
        ("POST", p) if p == format!("{base}/save") => {
            match serde_json::from_slice::<Choice>(&req.body) {
                Ok(c) if c.offset.unsigned_abs() <= 100_000 => {
                    on_save(c);
                    respond(&mut stream, "200 OK", "text/plain; charset=utf-8", b"ok")
                }
                _ => respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    b"ungueltig",
                ),
            }
        }
        _ => respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"",
        ),
    }
}

/// Text für HTML entschärfen (Namen kommen u. a. vom anderen Rechner).
fn esc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '&' => "&amp;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn page(s: &Snapshot) -> String {
    // JSON in <script>: "<" maskieren, damit kein </script> entstehen kann.
    let data = serde_json::to_string(s)
        .unwrap_or_else(|_| "{}".into())
        .replace('<', "\\u003c");
    PAGE.replace("__DATA__", &data)
        .replace("__LOCAL__", &esc(&s.local_name))
        .replace("__PEER__", &esc(&s.peer_name))
}

const PAGE: &str = r#"<!doctype html>
<html lang="de">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Anordnung – Device Connector</title>
<style>
  :root { --bg:#f5f5f7; --fg:#1d1d1f; --muted:#6e6e73; --card:#fff; --line:#d2d2d7;
          --local:#0a84ff; --peer:#34c759; --shadow:0 1px 3px rgba(0,0,0,.12); }
  @media (prefers-color-scheme: dark) {
    :root { --bg:#1c1c1e; --fg:#f5f5f7; --muted:#a1a1a6; --card:#2c2c2e; --line:#3a3a3c; }
  }
  * { box-sizing:border-box; }
  body { margin:0; font:15px/1.45 -apple-system, "Segoe UI", system-ui, sans-serif;
         background:var(--bg); color:var(--fg); }
  main { max-width:820px; margin:0 auto; padding:24px 16px 40px; }
  h1 { font-size:20px; margin:0 0 4px; }
  p.hint { color:var(--muted); margin:0 0 16px; }
  #stage { position:relative; height:360px; background:var(--card); border:1px solid var(--line);
           border-radius:12px; box-shadow:var(--shadow); overflow:hidden; touch-action:none; }
  .scr { position:absolute; border-radius:6px; display:flex; flex-direction:column;
         align-items:center; justify-content:center; color:#fff; user-select:none;
         text-align:center; padding:4px; font-weight:600; }
  .scr small { font-weight:400; opacity:.9; }
  #local { background:var(--local); }
  #peer { background:var(--peer); cursor:grab; }
  #peer.drag { cursor:grabbing; box-shadow:0 0 0 3px rgba(52,199,89,.35); }
  .row { display:flex; gap:12px; align-items:center; flex-wrap:wrap; margin-top:16px; }
  label { display:flex; gap:8px; align-items:center; }
  button { font:inherit; padding:8px 18px; border-radius:8px; border:0; background:var(--local);
           color:#fff; cursor:pointer; }
  button.sec { background:transparent; color:var(--local); border:1px solid var(--line); }
  #msg { color:var(--muted); min-height:1.4em; }
</style>
</head>
<body>
<main>
  <h1>Bildschirme anordnen</h1>
  <p class="hint">Ziehe den grünen Bildschirm (__PEER__) an die Stelle, an der er neben __LOCAL__ steht. Seite und Höhe übernehmen beide Rechner.</p>
  <div id="stage">
    <div id="local" class="scr">__LOCAL__<small id="lsize"></small></div>
    <div id="peer" class="scr">__PEER__<small id="psize"></small></div>
  </div>
  <div class="row">
    <label><input type="checkbox" id="stretch"> Ganze Höhe auf ganze Höhe (Zeiger wird gestreckt statt Pixel 1:1)</label>
  </div>
  <div class="row">
    <button id="save">Übernehmen</button>
    <button id="top" class="sec">Oben bündig</button>
    <button id="mid" class="sec">Mittig</button>
    <button id="bot" class="sec">Unten bündig</button>
    <span id="msg"></span>
  </div>
</main>
<script>
const D = __DATA__;
const stage = document.getElementById('stage');
const L = document.getElementById('local'), P = document.getElementById('peer');
const stretch = document.getElementById('stretch'), msg = document.getElementById('msg');
let side = D.side, offset = D.offset, scale = 1;
const [lw, lh] = D.local, [pw, ph] = D.peer;
document.getElementById('lsize').textContent = Math.round(lw) + ' × ' + Math.round(lh);
document.getElementById('psize').textContent = Math.round(pw) + ' × ' + Math.round(ph) +
  (D.peer_guessed ? ' (geschätzt)' : '');
stretch.checked = D.stretch;

function layout() {
  const W = stage.clientWidth, H = stage.clientHeight;
  const peerH = stretch.checked ? lh : ph;
  const totalW = lw + pw, totalH = Math.max(lh, peerH) + Math.abs(offset) + lh * 0.2;
  scale = Math.min((W - 40) / totalW, (H - 40) / totalH);
  const lx = side === 'Right' ? (W - totalW * scale) / 2 : (W - totalW * scale) / 2 + pw * scale;
  const top = (H - Math.max(lh, peerH) * scale) / 2 - (stretch.checked ? 0 : offset * scale / 2);
  const ly = Math.max(10, Math.min(H - lh * scale - 10, top));
  place(L, lx, ly, lw * scale, lh * scale);
  const px = side === 'Right' ? lx + lw * scale : lx - pw * scale;
  const py = stretch.checked ? ly : ly + offset * scale;
  place(P, px, py, pw * scale, (stretch.checked ? lh : ph) * scale);
}
function place(el, x, y, w, h) {
  el.style.left = x + 'px'; el.style.top = y + 'px';
  el.style.width = w + 'px'; el.style.height = h + 'px';
}
function clampOffset(o) {
  // Mindestens ein Pixel Überlappung, sonst gäbe es keine gemeinsame Kante.
  return Math.round(Math.max(-(ph - 1), Math.min(lh - 1, o)));
}
function snap(o) {
  const cands = [0, (lh - ph) / 2, lh - ph];
  for (const c of cands) if (Math.abs(o - c) < Math.max(lh, ph) * 0.02) return Math.round(c);
  return o;
}
let drag = null;
P.addEventListener('pointerdown', e => {
  drag = { x: e.clientX, y: e.clientY, off: offset, side: side };
  P.classList.add('drag'); P.setPointerCapture(e.pointerId); msg.textContent = '';
});
P.addEventListener('pointermove', e => {
  if (!drag) return;
  const lr = L.getBoundingClientRect();
  const cx = e.clientX;
  side = cx > lr.left + lr.width / 2 ? 'Right' : 'Left';
  if (!stretch.checked) offset = clampOffset(snap(drag.off + (e.clientY - drag.y) / scale));
  layout();
});
const end = () => { if (drag) { drag = null; P.classList.remove('drag'); } };
P.addEventListener('pointerup', end); P.addEventListener('pointercancel', end);
stretch.addEventListener('change', layout);
document.getElementById('top').onclick = () => { stretch.checked = false; offset = 0; layout(); };
document.getElementById('mid').onclick = () => { stretch.checked = false; offset = clampOffset((lh - ph) / 2); layout(); };
document.getElementById('bot').onclick = () => { stretch.checked = false; offset = clampOffset(lh - ph); layout(); };
document.getElementById('save').onclick = async () => {
  try {
    const r = await fetch(location.pathname + '/save', { method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ side, stretch: stretch.checked, offset }) });
    msg.textContent = r.ok ? 'Gespeichert. Du kannst dieses Fenster schließen.' : 'Speichern fehlgeschlagen.';
  } catch (e) { msg.textContent = 'Device Connector ist nicht erreichbar. Bitte im Menü neu öffnen.'; }
};
addEventListener('resize', layout);
layout();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot::new(
            "Dieser Mac".into(),
            "PC <b>".into(),
            (1920.0, 1244.0),
            Some((1920.0, 1080.0)),
            Side::Right,
            Align::Center,
        )
    }

    #[test]
    fn choice_becomes_named_align_when_it_matches() {
        let (l, p) = (1244.0, 1080.0);
        let c = |offset, stretch| Choice {
            side: Side::Right,
            stretch,
            offset,
        };
        assert_eq!(c(0, false).align(l, p), Align::Top);
        assert_eq!(c(82, false).align(l, p), Align::Center);
        assert_eq!(c(164, false).align(l, p), Align::Bottom);
        assert_eq!(c(37, false).align(l, p), Align::Offset(37));
        assert_eq!(c(37, true).align(l, p), Align::Stretch);
    }

    #[test]
    fn snapshot_offset_from_align() {
        assert_eq!(snap().offset, 82);
        assert!(!snap().stretch);
    }

    #[test]
    fn page_escapes_names() {
        let p = page(&snap());
        assert!(!p.contains("PC <b>"));
        assert!(p.contains("PC &lt;b&gt;"));
        assert!(!p.contains("__DATA__"));
    }

    fn http(url: &str, req: &str) -> String {
        let addr = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn server_serves_page_and_accepts_choice_only_with_token() {
        let (tx, rx) = std::sync::mpsc::channel();
        let url = start(
            snap(),
            Box::new(move |c| {
                let _ = tx.send(c);
            }),
        )
        .unwrap();
        let path = url.split_once("127.0.0.1:").unwrap().1;
        let path = &path[path.find('/').unwrap()..];
        let r = http(&url, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("Bildschirme anordnen"));
        let r = http(&url, "GET /falsch HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let body = r#"{"side":"Left","stretch":false,"offset":-40}"#;
        let r = http(
            &url,
            &format!(
                "POST /falsch/save HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let r = http(
            &url,
            &format!(
                "POST {path}/save HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        let c = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            c,
            Choice {
                side: Side::Left,
                stretch: false,
                offset: -40
            }
        );
    }
}
