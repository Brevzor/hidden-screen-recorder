use std::{env, fs, path::PathBuf};

use resvg::{tiny_skia, usvg};

fn main() {
    println!("cargo:rerun-if-changed=docs/images/logo.svg");
    println!("cargo:rerun-if-changed=build.rs");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let svg = fs::read("docs/images/logo.svg").expect("read application logo");
    let tree =
        usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("parse application logo");
    let sizes = [16_u32, 20, 24, 32, 48, 64, 128, 256];
    let frames: Vec<_> = sizes
        .iter()
        .map(|&size| {
            let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("allocate icon image");
            let transform = tiny_skia::Transform::from_scale(
                size as f32 / tree.size().width(),
                size as f32 / tree.size().height(),
            );
            resvg::render(&tree, transform, &mut pixmap.as_mut());
            pixmap.encode_png().expect("encode application icon")
        })
        .collect();
    fs::write(
        output.join("app-icon.png"),
        frames.last().expect("icon frame"),
    )
    .expect("write window icon");

    // ICO directory followed by PNG frames (supported by Windows 10/11).
    let mut ico = Vec::new();
    ico.extend_from_slice(&0_u16.to_le_bytes());
    ico.extend_from_slice(&1_u16.to_le_bytes());
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, png) in sizes.iter().zip(&frames) {
        let dimension = if size == 256 { 0 } else { size as u8 };
        ico.extend_from_slice(&[dimension, dimension, 0, 0]);
        ico.extend_from_slice(&1_u16.to_le_bytes());
        ico.extend_from_slice(&32_u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in &frames {
        ico.extend_from_slice(png);
    }
    let icon_path = output.join("app-icon.ico");
    fs::write(&icon_path, ico).expect("write executable icon");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon(icon_path.to_str().expect("UTF-8 icon path"))
            .compile()
            .expect("embed application icon with Windows SDK resource compiler");
    }
}
