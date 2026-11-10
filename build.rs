fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}/assets/app.manifest");
    }
    embed_resource::compile("assets/app.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
