//! Embeds the Windows resources: icon, VERSIONINFO (product, version, author) and a manifest.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/winstats.ico");
    println!("cargo:rerun-if-changed=assets/winstats.manifest");
    println!("cargo:rerun-if-changed=build.rs");

    let version = env!("CARGO_PKG_VERSION");
    // build year for the copyright line, without pulling in a date crate
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let year = 1970 + (secs as f64 / 86400.0 / 365.2425) as u64;
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/winstats.ico")
        .set_manifest_file("assets/winstats.manifest")
        .set("ProductName", "winstats")
        .set("FileDescription", "System resource widgets embedded in the Windows taskbar")
        .set("ProductVersion", version)
        .set("FileVersion", version)
        .set("CompanyName", "Oleksandr Zhabotynskyi")
        .set("LegalCopyright", &format!("Copyright \u{00A9} {year} Oleksandr Zhabotynskyi"))
        .set("OriginalFilename", "winstats.exe")
        .set("InternalName", "winstats");
    if let Err(e) = res.compile() {
        println!("cargo:warning=windows resources not embedded: {e}");
    }
}
