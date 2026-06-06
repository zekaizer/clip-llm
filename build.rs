use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let font_path = "assets/fonts/D2Coding.ttf";
    let out_path = Path::new(&out_dir).join("D2Coding.ttf.zst");

    let data = fs::read(font_path).expect("failed to read font file");
    let compressed = zstd::encode_all(&data[..], 19).expect("failed to compress font");

    let original = data.len();
    let result = compressed.len();
    println!(
        "cargo:warning=D2Coding font: {original} -> {result} bytes ({:.0}% reduction)",
        (1.0 - result as f64 / original as f64) * 100.0
    );

    fs::write(&out_path, compressed).expect("failed to write compressed font");
    println!("cargo:rerun-if-changed={font_path}");

    // Embed the app icon into the Windows executable (target-gated; a no-op for
    // other targets). A failure (e.g. no resource compiler) only warns.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app-icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app-icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows app icon: {e}");
        }
    }
}
