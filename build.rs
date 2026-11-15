// Build script:
//  * embeds the app icon and version information into swiftpdf.exe (Windows)
//  * if `pdfium.dll` sits in the project root, bakes it into the executable
//    (enables the `embedded_pdfium` cfg) so the .exe works as a single file.
//
// Nothing here can break the build: a failure to embed resources only prints a warning.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=pdfium.dll");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rustc-check-cfg=cfg(embedded_pdfium)");

    if std::path::Path::new("pdfium.dll").exists() {
        println!("cargo:rustc-cfg=embedded_pdfium");
    }

    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").map_or(false, |os| os == "windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            if let Err(e) = res.compile() {
                println!("cargo:warning=could not embed the Windows icon/version info: {e}");
            }
        }
    }
}
