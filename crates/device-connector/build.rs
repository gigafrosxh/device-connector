//! macOS: Core Audio schwach binden. Die Prozess-Tap-Funktionen für die
//! Tonaufnahme gibt es erst ab macOS 14.2; ohne schwache Bindung startete
//! die App auf älteren Systemen gar nicht. Aufgerufen werden sie nur nach
//! einer Versionsprüfung (`audio::backend::check_os`).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-weak_framework,CoreAudio");
    }
}
