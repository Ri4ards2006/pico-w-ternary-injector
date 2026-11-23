//! Build script:
//! 1. Places `memory.x` on the linker search path and adds the cortex-m-rt /
//!    embassy-rp / defmt linker scripts for the firmware binary.
//! 2. Injects WiFi credentials and the TCP port at compile time from the
//!    process environment or a git-ignored `.env` file (env wins), so no
//!    network configuration ever lives in the source tree.
use std::{collections::HashMap, env, fs, path::PathBuf};

const KEYS: [&str; 3] = ["WIFI_SSID", "WIFI_PSK", "TCP_PORT"];
const DEFAULT_TCP_PORT: &str = "7878";

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).expect("memory.x missing");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=.env");
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    // Host-side unit tests (`cargo test-host`) need no credentials.
    if !env::var("TARGET").unwrap_or_default().starts_with("thumbv6m") {
        return;
    }

    let dotenv = parse_dotenv(&fs::read_to_string(".env").unwrap_or_default());
    for key in KEYS {
        println!("cargo:rerun-if-env-changed={key}");
        let val = env::var(key).ok().or_else(|| dotenv.get(key).cloned());
        let val = match (key, val) {
            ("TCP_PORT", None) => DEFAULT_TCP_PORT.to_owned(),
            ("TCP_PORT", Some(v)) => {
                v.parse::<u16>().ok().filter(|p| *p != 0).unwrap_or_else(|| panic!("TCP_PORT={v} is not a valid port"));
                v
            }
            (_, Some(v)) => v,
            (_, None) => panic!("{key} not set: `cp .env.example .env` and fill it in (or export {key})"),
        };
        println!("cargo:rustc-env={key}={val}");
    }
}

fn parse_dotenv(src: &str) -> HashMap<String, String> {
    src.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
            (k.trim().to_owned(), v.to_owned())
        })
        .collect()
}
