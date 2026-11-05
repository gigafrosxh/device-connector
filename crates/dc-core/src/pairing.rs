//! Kopplungscode: ein zufälliger Code wie `K7QM-3XPA-9TRW`, der auf beiden
//! Rechnern gleich eingestellt sein muss. Aus ihm wird der Schlüssel für die
//! verschlüsselte Verbindung abgeleitet.

use rand::Rng;
use sha2::{Digest, Sha256};

/// Ohne leicht verwechselbare Zeichen (0/O, 1/I/L).
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const GROUPS: usize = 3;
const GROUP_LEN: usize = 4;

pub fn generate() -> String {
    let mut rng = rand::thread_rng();
    let mut groups = Vec::new();
    for _ in 0..GROUPS {
        let g: String = (0..GROUP_LEN)
            .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
            .collect();
        groups.push(g);
    }
    groups.join("-")
}

/// Großschreibung, Trennzeichen und Leerzeichen entfernen, O→0-Verwechsler korrigieren.
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .map(|c| match c {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

pub fn is_valid(code: &str) -> bool {
    let n = normalize(code);
    n.len() == GROUPS * GROUP_LEN && n.bytes().all(|b| ALPHABET.contains(&b))
}

/// 32-Byte-Schlüssel für Noise (PSK).
pub fn derive_psk(code: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"device-connector/pairing/v1/");
    h.update(normalize(code).as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_are_valid() {
        for _ in 0..100 {
            let c = generate();
            assert_eq!(c.len(), 14);
            assert!(is_valid(&c), "{c}");
        }
    }

    #[test]
    fn formatting_does_not_matter() {
        assert_eq!(derive_psk("k7qm-3xpa-9trw"), derive_psk("K7QM 3XPA 9TRW"));
        assert_ne!(derive_psk("K7QM-3XPA-9TRW"), derive_psk("K7QM-3XPA-9TRX"));
    }
}
