//! Tastencodes. Übertragen wird die *physische* Taste als USB-HID Usage-ID
//! (Seite 0x07). Jede Seite übersetzt in ihr eigenes Format:
//!
//! * Windows: Scancode Set 1, erweiterte Tasten als `0xE0xx`
//! * macOS:   virtueller Keycode (`kVK_*` aus `Events.h`)
//!
//! Das Zeichen entsteht erst auf dem Zielrechner durch dessen Tastaturlayout,
//! deshalb sollten beide Rechner dasselbe Layout eingestellt haben.

pub const NONE: u16 = 0xFFFF;

pub mod hid {
    pub const ESCAPE: u16 = 0x29;
    pub const H: u16 = 0x0B;
    pub const LEFT_CTRL: u16 = 0xE0;
    pub const LEFT_SHIFT: u16 = 0xE1;
    pub const LEFT_ALT: u16 = 0xE2;
    pub const LEFT_GUI: u16 = 0xE3;
    pub const RIGHT_CTRL: u16 = 0xE4;
    pub const RIGHT_SHIFT: u16 = 0xE5;
    pub const RIGHT_ALT: u16 = 0xE6;
    pub const RIGHT_GUI: u16 = 0xE7;
    pub const CAPS_LOCK: u16 = 0x39;
}

/// (HID, Windows-Scancode, macOS-Keycode)
const TABLE: &[(u16, u16, u16)] = &[
    // Buchstaben
    (0x04, 0x1E, 0x00), // A
    (0x05, 0x30, 0x0B), // B
    (0x06, 0x2E, 0x08), // C
    (0x07, 0x20, 0x02), // D
    (0x08, 0x12, 0x0E), // E
    (0x09, 0x21, 0x03), // F
    (0x0A, 0x22, 0x05), // G
    (0x0B, 0x23, 0x04), // H
    (0x0C, 0x17, 0x22), // I
    (0x0D, 0x24, 0x26), // J
    (0x0E, 0x25, 0x28), // K
    (0x0F, 0x26, 0x25), // L
    (0x10, 0x32, 0x2E), // M
    (0x11, 0x31, 0x2D), // N
    (0x12, 0x18, 0x1F), // O
    (0x13, 0x19, 0x23), // P
    (0x14, 0x10, 0x0C), // Q
    (0x15, 0x13, 0x0F), // R
    (0x16, 0x1F, 0x01), // S
    (0x17, 0x14, 0x11), // T
    (0x18, 0x16, 0x20), // U
    (0x19, 0x2F, 0x09), // V
    (0x1A, 0x11, 0x0D), // W
    (0x1B, 0x2D, 0x07), // X
    (0x1C, 0x15, 0x10), // Y (deutsch: Z)
    (0x1D, 0x2C, 0x06), // Z (deutsch: Y)
    // Ziffernreihe
    (0x1E, 0x02, 0x12), // 1
    (0x1F, 0x03, 0x13), // 2
    (0x20, 0x04, 0x14), // 3
    (0x21, 0x05, 0x15), // 4
    (0x22, 0x06, 0x17), // 5
    (0x23, 0x07, 0x16), // 6
    (0x24, 0x08, 0x1A), // 7
    (0x25, 0x09, 0x1C), // 8
    (0x26, 0x0A, 0x19), // 9
    (0x27, 0x0B, 0x1D), // 0
    // Steuer- und Satzzeichen
    (0x28, 0x1C, 0x24), // Enter
    (0x29, 0x01, 0x35), // Escape
    (0x2A, 0x0E, 0x33), // Backspace
    (0x2B, 0x0F, 0x30), // Tab
    (0x2C, 0x39, 0x31), // Leertaste
    (0x2D, 0x0C, 0x1B), // - (deutsch: ß)
    (0x2E, 0x0D, 0x18), // = (deutsch: ´)
    (0x2F, 0x1A, 0x21), // [ (deutsch: Ü)
    (0x30, 0x1B, 0x1E), // ] (deutsch: +)
    (0x31, 0x2B, 0x2A), // \ (ANSI)
    (0x32, 0x2B, 0x2A), // # (ISO, gleiche Position)
    (0x33, 0x27, 0x29), // ; (deutsch: Ö)
    (0x34, 0x28, 0x27), // ' (deutsch: Ä)
    (0x35, 0x29, 0x32), // ` (deutsch: ^)
    (0x36, 0x33, 0x2B), // ,
    (0x37, 0x34, 0x2F), // .
    (0x38, 0x35, 0x2C), // / (deutsch: -)
    (0x39, 0x3A, 0x39), // Feststelltaste
    (0x64, 0x56, 0x0A), // < > (ISO-Taste neben linkem Shift)
    // Funktionstasten
    (0x3A, 0x3B, 0x7A), // F1
    (0x3B, 0x3C, 0x78), // F2
    (0x3C, 0x3D, 0x63), // F3
    (0x3D, 0x3E, 0x76), // F4
    (0x3E, 0x3F, 0x60), // F5
    (0x3F, 0x40, 0x61), // F6
    (0x40, 0x41, 0x62), // F7
    (0x41, 0x42, 0x64), // F8
    (0x42, 0x43, 0x65), // F9
    (0x43, 0x44, 0x6D), // F10
    (0x44, 0x57, 0x67), // F11
    (0x45, 0x58, 0x6F), // F12
    (0x68, 0x64, 0x69), // F13
    (0x69, 0x65, 0x6B), // F14
    (0x6A, 0x66, 0x71), // F15
    // Navigation
    (0x46, 0xE037, NONE), // Druck
    (0x47, 0x46, NONE),   // Rollen
    (0x49, 0xE052, 0x72), // Einfg (Mac: Help)
    (0x4A, 0xE047, 0x73), // Pos1
    (0x4B, 0xE049, 0x74), // Bild hoch
    (0x4C, 0xE053, 0x75), // Entf
    (0x4D, 0xE04F, 0x77), // Ende
    (0x4E, 0xE051, 0x79), // Bild runter
    (0x4F, 0xE04D, 0x7C), // Pfeil rechts
    (0x50, 0xE04B, 0x7B), // Pfeil links
    (0x51, 0xE050, 0x7D), // Pfeil runter
    (0x52, 0xE048, 0x7E), // Pfeil hoch
    // Ziffernblock
    (0x53, 0x45, 0x47),   // Num (Mac: Clear)
    (0x54, 0xE035, 0x4B), // /
    (0x55, 0x37, 0x43),   // *
    (0x56, 0x4A, 0x4E),   // -
    (0x57, 0x4E, 0x45),   // +
    (0x58, 0xE01C, 0x4C), // Enter
    (0x59, 0x4F, 0x53),   // 1
    (0x5A, 0x50, 0x54),   // 2
    (0x5B, 0x51, 0x55),   // 3
    (0x5C, 0x4B, 0x56),   // 4
    (0x5D, 0x4C, 0x57),   // 5
    (0x5E, 0x4D, 0x58),   // 6
    (0x5F, 0x47, 0x59),   // 7
    (0x60, 0x48, 0x5B),   // 8
    (0x61, 0x49, 0x5C),   // 9
    (0x62, 0x52, 0x52),   // 0
    (0x63, 0x53, 0x41),   // ,
    (0x67, 0x59, 0x51),   // =
    (0x65, 0xE05D, NONE), // Kontextmenü
    // Modifier
    (0xE0, 0x1D, 0x3B),   // Strg links
    (0xE1, 0x2A, 0x38),   // Shift links
    (0xE2, 0x38, 0x3A),   // Alt / Option links
    (0xE3, 0xE05B, 0x37), // Windows / Cmd links
    (0xE4, 0xE01D, 0x3E), // Strg rechts
    (0xE5, 0x36, 0x3C),   // Shift rechts
    (0xE6, 0xE038, 0x3D), // AltGr / Option rechts
    (0xE7, 0xE05C, 0x36), // Windows / Cmd rechts
];

pub fn hid_to_win(hid: u16) -> Option<u16> {
    TABLE
        .iter()
        .find(|e| e.0 == hid)
        .map(|e| e.1)
        .filter(|&c| c != NONE)
}

pub fn win_to_hid(scancode: u16) -> Option<u16> {
    // 0x2B ist auf ANSI \ und auf ISO #; wir melden HID 0x31, beide Seiten
    // landen wieder auf derselben physischen Taste.
    TABLE.iter().find(|e| e.1 == scancode).map(|e| e.0)
}

pub fn hid_to_mac(hid: u16) -> Option<u16> {
    TABLE
        .iter()
        .find(|e| e.0 == hid)
        .map(|e| e.2)
        .filter(|&c| c != NONE)
}

pub fn mac_to_hid(keycode: u16) -> Option<u16> {
    TABLE
        .iter()
        .find(|e| e.2 == keycode && e.2 != NONE)
        .map(|e| e.0)
}

pub fn is_modifier(hid: u16) -> bool {
    (0xE0..=0xE7).contains(&hid)
}

/// Strg ⇄ Cmd/Windows tauschen, damit Strg+C auf dem PC am Mac zu Cmd+C wird
/// (und umgekehrt).
pub fn swap_ctrl_gui(hid: u16) -> u16 {
    match hid {
        hid::LEFT_CTRL => hid::LEFT_GUI,
        hid::LEFT_GUI => hid::LEFT_CTRL,
        hid::RIGHT_CTRL => hid::RIGHT_GUI,
        hid::RIGHT_GUI => hid::RIGHT_CTRL,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for e in TABLE {
            assert!(seen.insert(e.0), "HID {:#x} doppelt", e.0);
        }
    }

    #[test]
    fn mac_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for e in TABLE.iter().filter(|e| e.2 != NONE) {
            // 0x2A ist absichtlich für ANSI-\ und ISO-# gleich.
            if e.0 == 0x32 {
                continue;
            }
            assert!(seen.insert(e.2), "mac {:#x} doppelt", e.2);
        }
    }

    #[test]
    fn roundtrip_letters_and_modifiers() {
        for hid in (0x04..=0x1D).chain(0xE0..=0xE7) {
            let w = hid_to_win(hid).unwrap();
            assert_eq!(win_to_hid(w), Some(hid));
            let m = hid_to_mac(hid).unwrap();
            assert_eq!(mac_to_hid(m), Some(hid));
        }
    }

    #[test]
    fn mac_a_to_windows_a() {
        // kVK_ANSI_A = 0 → HID 0x04 → Scancode 0x1E
        assert_eq!(mac_to_hid(0x00).and_then(hid_to_win), Some(0x1E));
    }

    #[test]
    fn swap() {
        assert_eq!(swap_ctrl_gui(hid::LEFT_CTRL), hid::LEFT_GUI);
        assert_eq!(swap_ctrl_gui(0x04), 0x04);
    }
}
