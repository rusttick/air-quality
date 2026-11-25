use std::env;
use std::fs::File;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // Put memory.x in the linker search path
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());

    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");

    // SSID, PASSWORD and IP are compiled in by env!() in src/main.rs. Check
    // their values here: a bad one would otherwise only show up as a panic on
    // the device at boot. (A missing variable is reported by env!() itself.)
    for var in ["SSID", "PASSWORD", "IP"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    if let Ok(ssid) = env::var("SSID") {
        assert!(
            (1..=32).contains(&ssid.len()),
            "SSID must be 1 to 32 bytes long, got {}",
            ssid.len()
        );
    }
    if let Ok(password) = env::var("PASSWORD") {
        assert!(
            (8..=63).contains(&password.len()),
            "PASSWORD must be a WPA2 passphrase of 8 to 63 bytes, got {}",
            password.len()
        );
    }
    if let Ok(ip) = env::var("IP") {
        let addr: Ipv4Addr = ip
            .parse()
            .unwrap_or_else(|_| panic!("IP must be an IPv4 address like 192.168.1.50, got {ip:?}"));
        // The firmware assumes a /24 network with the gateway at x.x.x.1
        let host = addr.octets()[3];
        assert!(
            (2..=254).contains(&host),
            "IP must be a host address in a /24 network (last number 2 to 254; \
             x.x.x.1 is the gateway), got {ip}"
        );
    }
}
