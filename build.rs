fn main() {
    // Windows: rtp-audio.exe's icon and manifest. The manifest asks for Common Controls 6, which the
    // window (--features gui) imports from: without it Windows won't start the exe at all.
    println!("cargo:rerun-if-changed=windows");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("windows/rtp-audio.rc", embed_resource::NONE).manifest_required().expect("Windows resources");
    }
}
