// Embeds app.rc (the tray/taskbar icon) like build.zig's addWin32ResourceFile.
fn main() {
    println!("cargo:rerun-if-changed=../../assets/app.rc");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../assets/app.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
