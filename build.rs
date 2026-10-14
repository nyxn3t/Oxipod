//! Build script: Cargo runs this before compiling Oxipod.
//!
//! It finds libgpod with pkg-config and uses bindgen to turn libgpod's C
//! header into Rust declarations, written to `$OUT_DIR/gpod.rs`.

use std::env;
use std::path::PathBuf;

fn main() {
    // probe_library also tells Cargo to link against the library.
    let gpod = pkg_config::probe_library("libgpod-1.0")
        .expect("libgpod-1.0 not found, install libgpod-devel");
    // We call g_error_free ourselves, so link glib directly too.
    pkg_config::probe_library("glib-2.0").expect("glib-2.0 not found, install glib2-devel");

    let include_args = gpod
        .include_paths
        .iter()
        .map(|path| format!("-I{}", path.display()));

    let bindings = bindgen::Builder::default()
        .header("src/gpod/wrapper.h")
        .clang_args(include_args)
        // Only generate what we use; bindgen pulls in the types these need.
        .allowlist_function("itdb_.*")
        .allowlist_type("Itdb_.*")
        .allowlist_function("g_error_free")
        // Rebuild when the header changes.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen could not generate the libgpod bindings");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("gpod.rs"))
        .expect("could not write the libgpod bindings");
}
