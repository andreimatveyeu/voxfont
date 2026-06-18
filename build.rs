// Locate libfluidsynth and link against it. Dev headers and a pkg-config file
// are not required: this script discovers the shared object directly, adds its
// directory to the link search path, and bakes an rpath so the binary finds it
// at runtime.
use std::path::{Path, PathBuf};

fn main() {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // Explicit override wins.
    if let Ok(p) = std::env::var("FLUIDSYNTH_LIB_DIR") {
        dirs.push(PathBuf::from(p));
    }

    // Common cross-distribution locations for the linker-time symlink.
    // Canonicalize so the rpath points at the symlink's real target directory,
    // which the runtime loader can always reach.
    let candidates = [
        "/run/current-system/sw/lib/libfluidsynth.so",
        "/usr/lib/libfluidsynth.so",
        "/usr/lib/x86_64-linux-gnu/libfluidsynth.so",
        "/usr/local/lib/libfluidsynth.so",
        "/opt/homebrew/lib/libfluidsynth.dylib",
    ];
    for c in candidates {
        let p = Path::new(c);
        if p.exists() {
            if let Ok(real) = std::fs::canonicalize(p) {
                if let Some(d) = real.parent() {
                    dirs.push(d.to_path_buf());
                }
            }
            if let Some(d) = p.parent() {
                dirs.push(d.to_path_buf());
            }
        }
    }

    // Emit unique search paths + rpaths.
    let mut seen = std::collections::HashSet::new();
    for d in &dirs {
        if seen.insert(d.clone()) {
            println!("cargo:rustc-link-search=native={}", d.display());
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", d.display());
        }
    }

    println!("cargo:rustc-link-lib=dylib=fluidsynth");
    println!("cargo:rerun-if-env-changed=FLUIDSYNTH_LIB_DIR");
    println!("cargo:rerun-if-changed=build.rs");
}
