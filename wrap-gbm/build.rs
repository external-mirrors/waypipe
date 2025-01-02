fn depfile_to_cargo(path: &std::path::Path) {
    use std::io::Read;

    let mut depfile = std::fs::File::open(path).unwrap();
    let mut data = Vec::<u8>::new();
    depfile.read_to_end(&mut data).unwrap();
    // depfile contains the dependencies, in Make-style. Spaces in paths are escaped with '\ ',
    // and backslashes with '\\'. Other escaped chars (newlines, control characters) may break.
    let mut unescaped = Vec::new();
    let mut chunks: Vec<String> = Vec::new();
    assert!(!data.contains(&0));
    let mut scan = data.into_iter();
    loop {
        let Some(c) = scan.next() else {
            break;
        };
        if c == b'\\' {
            // TODO: how does Cargo handle escapes in path names? Or invalid utf8?
            let d = scan.next().unwrap();
            match d {
                b' ' => unescaped.push(b' '),
                b'\\' => unescaped.push(b'\\'),
                _ => panic!(),
            }
        } else if c == b' ' {
            chunks.push(std::str::from_utf8(&unescaped[..]).unwrap().into());
            unescaped.clear();
        } else {
            unescaped.push(c);
        }
    }
    chunks.push(std::str::from_utf8(&unescaped[..]).unwrap().into());

    for file in chunks.iter().skip(1) {
        println!("cargo:rerun-if-changed={}", file);
    }
}

fn main() {
    use std::ffi::OsStr;

    let libgbm = pkg_config::probe_library("gbm").unwrap();

    let mut includes = Vec::new();
    includes.extend_from_slice(&libgbm.include_paths[..]);

    let functions = &[
        "gbm_bo_create",
        "gbm_bo_destroy",
        "gbm_bo_get_modifier",
        "gbm_bo_get_fd",
        "gbm_bo_get_stride",
        "gbm_bo_import",
        "gbm_bo_map",
        "gbm_bo_unmap",
        "gbm_create_device",
        "gbm_device_get_backend_name",
        "gbm_device_is_format_supported",
        "gbm_device_destroy",
    ];

    let types = &[
        "gbm_import_fd_data",
        "gbm_bo_transfer_flags",
        "gbm_bo_flags",
    ];

    let vars = &["GBM_BO_IMPORT_FD"];

    let bindgen = "bindgen";

    let out_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bindings.rs");
    let dep_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("depfile");

    let mut args: Vec<&OsStr> = Vec::new();
    args.push(OsStr::new("--dynamic-loading"));
    args.push(OsStr::new("gbm"));
    args.push(OsStr::new("--dynamic-link-require-all"));
    args.push(OsStr::new("--rust-target"));
    args.push(OsStr::new("1.59"));
    args.push(OsStr::new("--no-doc-comments"));
    args.push(OsStr::new("--depfile"));
    args.push(dep_path.as_os_str());
    args.push(OsStr::new("--output"));
    args.push(out_path.as_os_str());

    for f in functions.iter() {
        args.push(OsStr::new("--allowlist-function"));
        args.push(OsStr::new(*f));
    }
    for f in vars.iter() {
        args.push(OsStr::new("--allowlist-var"));
        args.push(OsStr::new(*f));
    }
    for f in types.iter() {
        args.push(OsStr::new("--allowlist-type"));
        args.push(OsStr::new(*f));
    }
    args.push(OsStr::new("wrapper.h"));
    args.push(OsStr::new("--"));
    let inc_vec: Vec<String> = includes
        .iter()
        .map(|x| format!("-I{}", x.to_string_lossy()))
        .collect();
    for x in inc_vec.iter() {
        args.push(OsStr::new(x));
    }
    let mut child = std::process::Command::new(bindgen)
        .args(args)
        .spawn()
        .unwrap();
    let exit_status = child.wait().unwrap();
    assert!(exit_status.success());

    depfile_to_cargo(&dep_path);
}
