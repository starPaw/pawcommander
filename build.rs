fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let source = "src/native_drag/escape.m";
        println!("cargo:rerun-if-changed={source}");
        cc::Build::new()
            .file(source)
            .flag("-fobjc-arc")
            .compile("paw_drag_escape");
        println!("cargo:rustc-link-lib=framework=Carbon");
    }
}
