fn main() {
    // Embed the icon and version details in the Windows executables.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon_with_id("assets/seam.ico", "1")
            .set("ProductName", "Seam")
            .set("FileDescription", "Seam")
            .set(
                "LegalCopyright",
                "Copyright (c) The Seam contributors. MIT OR Apache-2.0",
            );
        resource.compile().expect("compiling Windows resources");
    }
    println!("cargo:rerun-if-changed=assets/seam.ico");
}
