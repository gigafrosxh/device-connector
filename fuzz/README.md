# Fuzzing (#32)

Fuzz-Ziele für alles, was Bytes oder Namen vom anderen Rechner verarbeitet.
Eigener Workspace, braucht Nightly und `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run frame_reader   corpus/frame_reader   -- -max_total_time=300
cargo +nightly fuzz run message_decode corpus/message_decode -- -max_total_time=300
cargo +nightly fuzz run sanitize       corpus/sanitize       -- -max_total_time=300
cargo +nightly fuzz run receiver       corpus/receiver       -- -max_total_time=300
```

| Ziel | Prüft |
|---|---|
| `frame_reader` | Rahmen-Parser: keine Panik, kein Stacküberlauf, Puffer wächst nicht über die Eingabe hinaus (#18) |
| `message_decode` | postcard-Dekodierung beliebiger Bytes: Fehler statt Panik, stabile Neukodierung |
| `sanitize` | Pfade vom anderen Rechner: abgelehnt oder nur harmlose relative Teile (#13) |
| `receiver` | Beliebige Datei-Nachrichtenfolgen: nichts außerhalb des Empfangsordners, keine `.part`-Reste nach Abbruch |

`corpus/*` enthält Regressionseingaben zu #13 und #18. Findet ein Lauf einen
Absturz, landet die Eingabe in `artifacts/<ziel>/`; sie gehört dann als
Datei ins passende `corpus/`-Verzeichnis und als fester Fall in
`crates/dc-core/tests/properties.rs` oder die Unit-Tests.

Ohne Nightly decken die Property-Tests in `crates/dc-core/tests/properties.rs`
dieselben Eigenschaften ab (laufen in CI mit `cargo test`). Längere Läufe:

```sh
PROPTEST_CASES=100000 cargo test -p dc-core --test properties --release
```
