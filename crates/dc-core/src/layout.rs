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
}

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
        }
    }

    pub fn step(&mut self, screen: &Rect, peer_side: Side, dx: f64, dy: f64) -> CursorStep {
        let nx = self.x + dx;
        let ny = (self.y + dy).clamp(screen.y, screen.bottom());
        let crossed = match peer_side {
            Side::Left => nx < screen.x,
            Side::Right => nx > screen.right(),
        };
        if crossed {
            return CursorStep::Exited {
                y_ratio: screen.y_ratio(ny),
            };
        }
        self.x = nx.clamp(screen.x, screen.right());
        self.y = ny;
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
