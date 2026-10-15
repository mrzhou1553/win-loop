fn main() {
    println!("cargo:rerun-if-changed=assets/win-loop.rc");
    println!("cargo:rerun-if-changed=assets/win-loop.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for("assets/win-loop.rc", ["win-loop"], embed_resource::NONE)
            .manifest_required()
            .expect("无法嵌入应用图标，请检查 Windows SDK 资源编译器");
    }
}
