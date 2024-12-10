fn main() {
    let lib = pkg_config::probe_library("liblz4").unwrap();

    println!("cargo:rustc-link-lib=lz4");

    let mut includes = Vec::new();
    includes.extend_from_slice(&lib.include_paths[..]);

    let functions: &[&str] = &[
        "LZ4_compressBound",
        "LZ4_sizeofState",
        "LZ4_sizeofStateHC",
        "LZ4_compress_fast_extState",
        "LZ4_compress_HC_extStateHC",
        "LZ4_decompress_safe",
    ];

    let types: &[&str] = &[];

    let vars: &[&str] = &[];

    let mut bindings = bindgen::Builder::default()
        .clang_args(
            includes
                .into_iter()
                .map(|x| format!("-I{}", x.to_string_lossy())),
        )
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .rust_target(bindgen::RustTarget::Stable_1_77);
    for f in functions {
        bindings = bindings.allowlist_function(f);
    }
    for t in types {
        bindings = bindings.allowlist_type(t);
    }
    for v in vars {
        bindings = bindings.allowlist_var(v);
    }

    let builder = bindings.generate().unwrap();
    let out_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    builder.write_to_file(out_path.join("bindings.rs")).unwrap()
}
