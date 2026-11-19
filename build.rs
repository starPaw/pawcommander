fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let source = "src/native_drag/escape.m";
        println!("cargo:rerun-if-changed={source}");
        cc::Build::new()
            .file(source)
            .flag("-fobjc-arc")
            .compile("paw_drag_escape");
        println!("cargo:rustc-link-lib=framework=Carbon");
        let pdf = "src/preview/pdf/macos.m";
        println!("cargo:rerun-if-changed={pdf}");
        cc::Build::new()
            .file(pdf)
            .flag("-fobjc-arc")
            .compile("paw_pdf");
        println!("cargo:rustc-link-lib=framework=PDFKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        let metadata = "src/preview/metadata/macos.m";
        println!("cargo:rerun-if-changed={metadata}");
        cc::Build::new()
            .file(metadata)
            .flag("-fobjc-arc")
            .compile("paw_image_metadata");
        println!("cargo:rustc-link-lib=framework=ImageIO");
        let media = "src/preview/media/macos.m";
        println!("cargo:rerun-if-changed={media}");
        cc::Build::new()
            .file(media)
            .flag("-fobjc-arc")
            .compile("paw_media");
        for framework in ["AVFoundation", "CoreMedia", "CoreVideo", "QuartzCore"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
