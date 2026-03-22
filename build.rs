use flate2::{write::GzEncoder, Compression};
use std::{env, fs, io::Write, path::PathBuf};

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn make_touch_icon(out: &PathBuf) {
    let svg_data = fs::read("src/favicon.svg").unwrap();

    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    let opt = resvg::usvg::Options {
        fontdb: std::sync::Arc::new(fontdb),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(&svg_data, &opt).unwrap();

    let size = 180u32;
    let scale = size as f32 / tree.size().width();
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap.save_png(out.join("apple-touch-icon.png")).unwrap();
}

fn main() {
    println!("cargo:rerun-if-changed=src/index.html");
    println!("cargo:rerun-if-changed=src/favicon.svg");

    let ssid = env::var("WIFI_SSID").unwrap();
    let password = env::var("WIFI_PASSWORD").unwrap();
    println!("cargo:rustc-env=WIFI_SSID={ssid}");
    println!("cargo:rustc-env=WIFI_PASSWORD={password}");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let html = fs::read("src/index.html").unwrap();
    fs::write(out.join("index.html.gz"), gzip(&html)).unwrap();

    make_touch_icon(&out);
}
