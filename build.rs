fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.ico");
    println!("cargo:rerun-if-changed=app.rc");
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}/app.manifest");
    }
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
