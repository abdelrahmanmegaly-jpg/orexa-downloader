// Embeds the Orexa icon and product name into the Windows executable.
fn main() {
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
