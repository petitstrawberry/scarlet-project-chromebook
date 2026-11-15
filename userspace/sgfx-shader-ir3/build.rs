// SPDX-License-Identifier: GPL-2.0-only
fn main() {
    for name in ["IR3_MESA_LIB_DIR", "SCARLET_NATIVE_RUNTIME"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if std::env::var_os("CARGO_FEATURE_MESA").is_none() {
        return;
    }
    let dir = std::env::var("IR3_MESA_LIB_DIR")
        .expect("run native/build.py, then set IR3_MESA_LIB_DIR to its lib directory");
    let archive = std::path::Path::new(&dir).join("libscarlet_ir3.a");
    assert!(
        archive.is_file(),
        "missing source-built Mesa compiler archive"
    );
    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=static:-bundle=scarlet_ir3");
    match std::env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "scarlet" => {
            let runtime =
                std::env::var("SCARLET_NATIVE_RUNTIME").expect("Scarlet native runtime required");
            println!("cargo:rustc-link-search=native={runtime}/lib");
            for lib in ["c++", "c++abi", "scarlet_cxx_support", "scarlet_c"] {
                println!("cargo:rustc-link-lib=static:-bundle={lib}");
            }
        }
        "macos" => {
            println!("cargo:rustc-link-lib=c++");
        }
        _ => {
            println!("cargo:rustc-link-lib=stdc++");
            println!("cargo:rustc-link-lib=m");
        }
    }
}
