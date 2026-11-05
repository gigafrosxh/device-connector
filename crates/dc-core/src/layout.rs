//! Bildschirmgeometrie: wo steht der andere Rechner, wann ist der Rand erreicht.

use serde::{Deserialize, Serialize};

/// Auf welcher Seite steht der andere Rechner?
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// Wie die Bildschirme beider Rechner in der Höhe zueinander stehen (#4).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// Ganze Höhe auf ganze Höhe (oben trifft oben, unten trifft unten).
    #[default]
    Stretch,
    /// Pixel 1:1, Oberkanten auf gleicher Höhe.
    Top,
    /// Pixel 1:1, Mitten auf gleicher Höhe.
    Center,
    /// Pixel 1:1, Unterkanten auf gleicher Höhe.
    Bottom,
    /// Pixel 1:1, frei verschoben (#36): Die Oberkante des anderen
    /// Bildschirms liegt so viele Pixel unter unserer Oberkante (negativ:
    /// darüber). Eingestellt per Ziehen im Anordnungsfenster.
    Offset(i32),
}

impl Align {
    /// Relative Höhe auf diesem Bildschirm (Höhe `h_from`) in die relative
    /// Höhe auf dem anderen (Höhe `h_to`) umrechnen. Außerhalb des
    /// gemeinsamen Bereichs wird an den Rand gesetzt statt zu blockieren,
    /// damit der Zeiger nie „hängen bleibt“.
    pub fn map(self, ratio: f32, h_from: f64, h_to: f64) -> f32 {
        if self == Align::Stretch || h_from <= 1.0 || h_to <= 1.0 {
            return ratio.clamp(0.0, 1.0);
        }
        let y = ratio.clamp(0.0, 1.0) as f64 * (h_from - 1.0);
        let shift = self.shift(h_from, h_to);
        (((y - shift) / (h_to - 1.0)).clamp(0.0, 1.0)) as f32
    }

    /// Wo die Oberkante des anderen Bildschirms (Höhe `h_to`) in unseren
    /// Koordinaten liegt (Höhe `h_from`). Bei `Stretch` 0.
    pub fn shift(self, h_from: f64, h_to: f64) -> f64 {
        match self {
            Align::Top | Align::Stretch => 0.0,
            Align::Center => (h_from - h_to) / 2.0,
            Align::Bottom => h_from - h_to,
            Align::Offset(o) => o as f64,
        }
    }

    /// Dieselbe Anordnung aus Sicht des anderen Rechners.
    pub fn mirrored(self) -> Align {
        match self {
            Align::Offset(o) => Align::Offset(o.saturating_neg()),
            a => a,
        }
    }
}

/// Rechteck in Bildschirmkoordinaten (Ursprung oben links, y wächst nach unten).
/// Bei mehreren Monitoren ist das die umschließende Fläche aller Monitore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w - 1.0
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h - 1.0
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Relative Höhe 0..=1 eines y-Werts.
    pub fn y_ratio(&self, y: f64) -> f32 {
        if self.h <= 1.0 {
            return 0.5;
        }
        (((y - self.y) / (self.h - 1.0)).clamp(0.0, 1.0)) as f32
    }

    pub fn y_from_ratio(&self, r: f32) -> f64 {
        self.y + (r.clamp(0.0, 1.0) as f64) * (self.h - 1.0)
    }

    /// x-Koordinate der Kante zur Seite `side`.
    pub fn edge_x(&self, side: Side) -> f64 {
        match side {
            Side::Left => self.x,
            Side::Right => self.right(),
        }
    }

    /// Ein paar Pixel nach innen versetzt, damit der Rand nicht sofort wieder auslöst.
    pub fn inset_x(&self, side: Side, px: f64) -> f64 {
        match side {
            Side::Left => self.x + px,
            Side::Right => self.right() - px,
        }
    }

    pub fn at_edge(&self, side: Side, x: f64) -> bool {
        match side {
            Side::Left => x <= self.x,
            Side::Right => x >= self.right(),
        }
    }

    pub fn distance_from_edge(&self, side: Side, x: f64) -> f64 {
        match side {
            Side::Left => x - self.x,
            Side::Right => self.right() - x,
        }
    }
}

/// Virtueller Cursor auf dem gesteuerten Rechner. Er sammelt die Bewegungen
/// des anderen Rechners und meldet, wann die Rückkehr-Kante überschritten ist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VirtualCursor {
    pub x: f64,
    pub y: f64,
    /// Rückweg erst scharf, wenn der Zeiger sich etwas von der Kante entfernt
    /// hat. Sonst springt er bei kleinem Zittern sofort wieder zurück.
    pub armed: bool,
    /// Vor dem Scharfwerden: wie weit schon gegen die Kante gedrückt wurde.
    pub push: f64,
}

/// So weit muss der Zeiger nach dem Betreten von der Kante weg, bevor er
/// wieder zurückwechseln kann.
pub const EXIT_ARM_PX: f64 = 6.0;

pub enum CursorStep {
    /// Cursor bleibt auf diesem Bildschirm.
    Moved { x: f64, y: f64 },
    /// Cursor hat die Kante zum anderen Rechner überschritten.
    Exited { y_ratio: f32 },
}

impl VirtualCursor {
    /// Cursor betritt den Bildschirm an der Kante zu `peer_side`.
    pub fn enter(screen: &Rect, peer_side: Side, y_ratio: f32) -> VirtualCursor {
        VirtualCursor {
            x: screen.inset_x(peer_side, 1.0),
            y: screen.y_from_ratio(y_ratio),
            armed: false,
            push: 0.0,
        }
    }

    pub fn step(&mut self, screen: &Rect, peer_side: Side, dx: f64, dy: f64) -> CursorStep {
        let nx = self.x + dx;
        let ny = (self.y + dy).clamp(screen.y, screen.bottom());
        let overshoot = match peer_side {
            Side::Left => screen.x - nx,
            Side::Right => nx - screen.right(),
        };
        // Kleines Zittern direkt nach dem Betreten zählt nicht. Wer weiter
        // gegen die Kante drückt (auch langsam), kommt nach EXIT_ARM_PX zurück.
        if overshoot > 0.0 {
            self.push += overshoot;
            if self.armed || self.push >= EXIT_ARM_PX {
                return CursorStep::Exited {
                    y_ratio: screen.y_ratio(ny),
                };
            }
        } else if overshoot < -1.0 {
            self.push = 0.0;
        }
        self.x = nx.clamp(screen.x, screen.right());
        self.y = ny;
        if !self.armed && screen.distance_from_edge(peer_side, self.x) >= EXIT_ARM_PX {
            self.armed = true;
        }
        CursorStep::Moved {
            x: self.x,
            y: self.y,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_roundtrip() {
        let r = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(r.y_ratio(0.0), 0.0);
        assert_eq!(r.y_ratio(1079.0), 1.0);
        let y = r.y_from_ratio(r.y_ratio(540.0));
        assert!((y - 540.0).abs() < 0.01);
    }

    #[test]
    fn negative_origin_multi_monitor() {
        // Zweiter Monitor links vom Hauptmonitor (Windows-Koordinaten).
        let r = Rect::new(-1920.0, 0.0, 3840.0, 1080.0);
        assert!(r.at_edge(Side::Left, -1920.0));
        assert!(!r.at_edge(Side::Left, -1919.0));
        assert!(r.at_edge(Side::Right, 1919.0));
    }

    #[test]
    fn align_maps_heights() {
        // Mac 900 hoch, PC 1080 hoch.
        let (mac, pc) = (900.0, 1080.0);
        assert_eq!(Align::Stretch.map(0.5, mac, pc), 0.5);
        // Oben bündig: oberste Zeile bleibt oben, unterste Mac-Zeile landet
        // bei 899 von 1079 am PC.
        assert_eq!(Align::Top.map(0.0, mac, pc), 0.0);
        let r = Align::Top.map(1.0, mac, pc);
        assert!((r as f64 * 1079.0 - 899.0).abs() < 0.5, "{r}");
        // Vom PC unten (außerhalb des Macs) → Mac-Unterkante.
        assert_eq!(Align::Top.map(1.0, pc, mac), 1.0);
        // Unten bündig: Unterkanten treffen sich.
        assert_eq!(Align::Bottom.map(1.0, mac, pc), 1.0);
        assert_eq!(Align::Bottom.map(0.0, pc, mac), 0.0);
        let r = Align::Bottom.map(0.0, mac, pc);
        assert!((r as f64 * 1079.0 - 180.0).abs() < 0.5, "{r}");
        // Mitte: Mitte trifft Mitte.
        let r = Align::Center.map(0.5, mac, pc);
        assert!((r - 0.5).abs() < 0.001, "{r}");
        // Hin und zurück im gemeinsamen Bereich verlustfrei.
        for i in 0..=10 {
            let r = i as f32 / 10.0;
            for a in [Align::Top, Align::Center, Align::Bottom] {
                let back = a.map(a.map(r, mac, pc), pc, mac);
                assert!((back - r).abs() < 0.002, "{a:?} {r} → {back}");
            }
        }
    }

    #[test]
    fn offset_maps_and_mirrors() {
        let (mac, pc) = (900.0, 1080.0);
        // PC-Oberkante 100 px unter der Mac-Oberkante.
        let a = Align::Offset(100);
        assert_eq!(a.map(0.0, mac, pc), 0.0, "oberhalb des PCs → Oberkante");
        let r = a.map(1.0, mac, pc);
        assert!((r as f64 * 1079.0 - 799.0).abs() < 0.5, "{r}");
        // Spiegel: vom PC aus liegt der Mac 100 px höher.
        assert_eq!(a.mirrored(), Align::Offset(-100));
        let back = a.mirrored().map(r, pc, mac);
        assert!((back - 1.0).abs() < 0.002, "{back}");
        // Offset(0) entspricht Top, Mitte entspricht Center.
        for i in 0..=10 {
            let r = i as f32 / 10.0;
            assert_eq!(Align::Offset(0).map(r, mac, pc), Align::Top.map(r, mac, pc));
            assert!(
                (Align::Offset(-90).map(r, mac, pc) - Align::Center.map(r, mac, pc)).abs() < 1e-6
            );
        }
        assert_eq!(Align::Center.mirrored(), Align::Center);
        assert_eq!(Align::Offset(i32::MIN).mirrored(), Align::Offset(i32::MAX));
    }

    #[test]
    fn cursor_exits_only_on_peer_side() {
        let s = Rect::new(0.0, 0.0, 1440.0, 900.0);
        let mut c = VirtualCursor::enter(&s, Side::Left, 0.5);
        assert_eq!(c.x, 1.0);
        // Bewegung nach rechts bis zur Gegenkante: kein Austritt, sondern Begrenzung.
        match c.step(&s, Side::Left, 5000.0, 0.0) {
            CursorStep::Moved { x, .. } => assert_eq!(x, 1439.0),
            CursorStep::Exited { .. } => panic!("falsche Kante"),
        }
        match c.step(&s, Side::Left, -1440.0, -2000.0) {
            CursorStep::Exited { y_ratio } => assert_eq!(y_ratio, 0.0),
            CursorStep::Moved { .. } => panic!("hätte austreten müssen"),
        }
    }
}
