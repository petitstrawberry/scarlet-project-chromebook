#![cfg(unix)]

use sgfx_backend_loader::{LoadedBackend, discover};
use std::{fs, sync::Arc};

#[test]
fn installed_manifest_discovers_and_loads_the_real_plugin() {
    let directory =
        std::env::temp_dir().join(format!("coachz-adreno-loader-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let extension = std::env::consts::DLL_EXTENSION;
    let library = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("libsgfx_scarlet_adreno.{extension}"));
    assert!(
        library.is_file(),
        "Cargo must build the cdylib alongside the integration test"
    );
    std::os::unix::fs::symlink(&library, directory.join("libsgfx_scarlet_adreno.so")).unwrap();
    fs::write(
        directory.join("scarlet-adreno.sgfx-driver"),
        include_str!("../scarlet-adreno.sgfx-driver"),
    )
    .unwrap();
    let directories = [directory.to_str().unwrap().to_owned()];
    let manifest = discover(&directories, "qcom-adreno", None).unwrap();
    let loaded = LoadedBackend::load(&manifest).unwrap();
    assert_eq!(loaded.name, "scarlet-adreno");
    assert_eq!(loaded.gpu_backend, "qcom-adreno");
    assert!(Arc::ptr_eq(
        &loaded,
        &LoadedBackend::load(&manifest).unwrap()
    ));
    assert!(discover(&directories, "virtio-gpu", None).is_err());
    assert!(discover(&directories, "qcom-adreno", Some("scarlet-virgl")).is_err());
    // Exercise a callable obtained through dlsym, without pretending the host
    // has Scarlet's GPU device. No Rust object crosses the library boundary.
    let mut receipt = sgfx_backend_abi::SubmitResult::default();
    unsafe {
        (loaded.api.submit)(core::ptr::null_mut(), core::ptr::null(), &mut receipt);
    }
    assert_eq!(receipt.error, sgfx_backend_abi::INVALID);
    assert_eq!(receipt.disposition, sgfx_backend_abi::REJECTED);
    assert!(receipt.receipt.is_null());
}
