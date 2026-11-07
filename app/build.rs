// Embeds the Orexa icon and product name into the Windows executable.
fn main() {
    // The Thmanyah Arabic font is licensed for use inside a compiled product but
    // not for redistribution as files, so it is not part of the public source.
    // Without it the app builds fine and uses the Windows Arabic fonts instead.
    println!("cargo:rustc-check-cfg=cfg(has_thmanyah)");
    println!("cargo:rerun-if-changed=assets/fonts");
    if std::path::Path::new("assets/fonts/thmanyahsans-Regular.otf").exists()
        && std::path::Path::new("assets/fonts/thmanyahsans-Medium.otf").exists()
        && std::path::Path::new("assets/fonts/thmanyahsans-Bold.otf").exists()
    {
        println!("cargo:rustc-cfg=has_thmanyah");
    }
    println!("cargo:rerun-if-changed=assets/orexa.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/orexa.ico");
        res.set("ProductName", "Orexa Downloader");
        res.set("FileDescription", "Orexa Downloader");
        res.set("CompanyName", "Orexa");
        res.set("OriginalFilename", "orexa-downloader.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {}", e);
        }
    }
}
