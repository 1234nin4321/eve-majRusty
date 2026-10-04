// Embeds app.rc (the tray/taskbar icon) like build.zig's addWin32ResourceFile.
fn main() {
    println!("cargo:rerun-if-changed=../../../app.rc");
    println!("cargo:rerun-if-changed=../../../icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../../app.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
