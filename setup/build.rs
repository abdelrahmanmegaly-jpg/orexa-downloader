// - Reads the app version from ../app/Cargo.toml (one place to change it).
// - Compresses the built app so the setup file stays small.
// - Gives the setup program the Orexa icon and product details.
use std::io::Write;

fn main() {
    println!("cargo:rerun-if-changed=../app/Cargo.toml");
    println!("cargo:rerun-if-changed=../extension");
    println!("cargo:rerun-if-changed=../app/assets/orexa.ico");
    println!("cargo:rerun-if-changed=../app/target/release/orexa-downloader.exe");

    let manifest = std::fs::read_to_string("../app/Cargo.toml").expect("read app Cargo.toml");
    let version = manifest
        .lines()
        .find_map(|l| {
            let l = l.trim();
            l.strip_prefix("version")
                .and_then(|r| r.split('"').nth(1))
                .map(|v| v.to_string())
        })
        .unwrap_or_else(|| "0.0.0".into());
    println!("cargo:rustc-env=APP_VERSION={}", version);

    let exe = std::fs::read("../app/target/release/orexa-downloader.exe")
        .expect("build the app first (cargo build --release in ../app)");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("app.exe.gz");
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&exe).unwrap();
    std::fs::write(out, enc.finish().unwrap()).unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../app/assets/orexa.ico");
        res.set("ProductName", "Orexa Downloader Setup");
        res.set("FileDescription", "Orexa Downloader Setup");
        res.set("CompanyName", "Orexa");
        res.set("ProductVersion", &version);
        res.set("FileVersion", &version);
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the setup icon: {}", e);
        }
    }
}
