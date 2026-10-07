fn main() {
    // The page is built by npm (see README.md) and embedded into the program.
    println!("cargo:rerun-if-changed=web/dist");
    if !std::path::Path::new("web/dist/index.html").exists() {
        panic!("the window's page isn't built: run `npm ci && npm run build` in src/gui/web first");
    }
    tauri_build::build();
}
