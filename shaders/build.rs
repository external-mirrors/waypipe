use std::env;
use std::ffi::OsStr;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    let paths = fs::read_dir("./").unwrap();

    let mut shaders = Vec::new();
    for p in paths {
        let q = p.unwrap();
        if q.path().extension() == Some(OsStr::new("glsl")) {
            shaders.push(q.path());
        }
    }

    /* No rerun-if directives -- these will track changes to
     * existing files, but not register any new shaders. */

    let compiler = "glslc";

    /* If parallelization is ever needed, use jobserver */
    shaders.sort();
    let mut contents = String::new();

    for shader in shaders {
        let args: &[&OsStr] = &[
            OsStr::new("-O"),
            OsStr::new("-fshader-stage=compute"),
            shader.as_os_str(),
            OsStr::new("-o"),
            OsStr::new("-"),
        ];
        let spirv = Command::new(compiler)
            .args(args)
            .output()
            .expect("Failed to compile file")
            .stdout;

        let s: &str = shader.file_stem().unwrap().to_str().unwrap();

        write!(
            &mut contents,
            "pub const {}: &[u32] = &[\n",
            s.to_uppercase()
        )
        .unwrap();

        assert!(spirv.len() % 4 == 0);
        for w in spirv.chunks_exact(4) {
            let block = u32::from_ne_bytes(w.try_into().unwrap());
            write!(&mut contents, "    {:#010x},\n", block).unwrap();
        }
        write!(&mut contents, "];\n").unwrap();
    }

    let generated_path = Path::new(&env::var("OUT_DIR").unwrap()).join("shaders.rs");
    fs::write(&generated_path, contents).unwrap();
}
