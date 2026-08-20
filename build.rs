fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/Longwatch.rc");
    println!("cargo:rerun-if-changed=packaging/windows/Longwatch.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for(
            "packaging/windows/Longwatch.rc",
            ["codex-longwatch"],
            embed_resource::NONE,
        )
        .manifest_optional()
        .unwrap();
    }
}
