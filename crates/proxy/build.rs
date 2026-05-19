use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/vpx_wrapper.h");

    let vpx = pkg_config::Config::new()
        .probe("vpx")
        .expect("libvpx is required for native VP9 frame extraction");

    let mut bindings = bindgen::Builder::default()
        .header("src/vpx_wrapper.h")
        .allowlist_function("vpx_codec_.*")
        .allowlist_function("vpx_codec_vp9_dx")
        .allowlist_type("vpx_.*")
        .allowlist_var("VPX_.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for include_path in vpx.include_paths {
        bindings = bindings.clang_arg(format!("-I{}", include_path.display()));
    }

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    bindings
        .generate()
        .expect("failed to generate libvpx bindings")
        .write_to_file(out_path.join("vpx_bindings.rs"))
        .expect("failed to write libvpx bindings");
}
