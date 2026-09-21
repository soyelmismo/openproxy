fn main() {
    println!("cargo:rerun-if-changed=c_src/laya_bridge.c");
    println!("cargo:rerun-if-changed=c_src/laya_bridge.h");
    println!("cargo:rerun-if-changed=c_src/onnxruntime_c_api.h");

    if std::env::var("CARGO_FEATURE_LAYA_ENGINE").is_ok() {
        cc::Build::new()
            .file("c_src/laya_bridge.c")
            .include("c_src")
            .opt_level(3)
            .warnings(false)
            .compile("laya_bridge");

        println!("cargo:rustc-link-lib=dl");
    }
}
