//! Renders `assets/icon.svg` at compile time into a multi-size Windows `.ico` and a 64x64 notification
//! bitmap in `OUT_DIR`, then embeds the icon, version info, and PerMonitorV2 manifest into the executable.

use std::env;
use std::fs;
use std::path::PathBuf;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity type="win32" name="TaskbarAudio" version="1.0.0.0"/>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
    </application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;

fn render_svg_png(tree: &resvg::usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("pixmap allocation");
    let svg_size = tree.size();
    let sx = size as f32 / svg_size.width();
    let sy = size as f32 / svg_size.height();
    let transform = resvg::tiny_skia::Transform::from_scale(sx, sy);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    pixmap.encode_png().expect("encode png from svg")
}

fn build_ico_from_svg(tree: &resvg::usvg::Tree, sizes: &[u32]) -> Vec<u8> {
    let pngs: Vec<Vec<u8>> = sizes.iter().map(|&s| render_svg_png(tree, s)).collect();
    let mut ico = Vec::new();
    // ICO header: reserved (0), type (1 = ICO), count
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    let mut offset = 6 + (16 * sizes.len()) as u32;
    for (i, &s) in sizes.iter().enumerate() {
        let data = &pngs[i];
        let dim = if s >= 256 { 0u8 } else { s as u8 };
        ico.push(dim);
        ico.push(dim);
        ico.push(0); // palette
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // color planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in &pngs {
        ico.extend_from_slice(data);
    }
    ico
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.svg");

    let svg_bytes = fs::read("assets/icon.svg").expect("failed to read assets/icon.svg");
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&svg_bytes, &opt).expect("invalid assets/icon.svg");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out_dir.join("icon.ico");
    let png64_path = out_dir.join("icon-64.png");

    fs::write(&ico_path, build_ico_from_svg(&tree, &[16, 32, 48, 256]))
        .expect("failed to write generated icon.ico");
    fs::write(&png64_path, render_svg_png(&tree, 64))
        .expect("failed to write generated icon-64.png");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().expect("ico_path"))
            .set_manifest(MANIFEST)
            .set("FileDescription", "Taskbar Audio")
            .set("ProductName", "Taskbar Audio")
            .set("LegalCopyright", "© 2026 Vinny. MIT License.");
        res.compile()
            .expect("failed to compile Windows resources (is the Windows SDK installed?)");
    }
}
