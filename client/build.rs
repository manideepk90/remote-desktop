fn main() {
    // Embed the icon and version details in the Windows executable. Building on
    // Windows must succeed at this; cross-checks from other systems just skip it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "Remote Desk Viewer");
        res.set("ProductName", "Remote Desk Viewer");
        res.set("LegalCopyright", "MIT License");
        match res.compile() {
            Ok(()) => {}
            Err(e) if cfg!(windows) => panic!("could not embed the Windows resources: {e}"),
            Err(e) => println!("cargo:warning=skipping the Windows icon when cross-compiling: {e}"),
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
