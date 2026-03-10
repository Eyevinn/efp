use std::env;
use std::path::PathBuf;

fn main() {
    // Rebuild if the vendored C++ source changes.
    println!("cargo:rerun-if-changed=../vendor/efp/ElasticFrameProtocol.cpp");
    println!("cargo:rerun-if-changed=../vendor/efp/ElasticFrameProtocol.h");
    println!("cargo:rerun-if-changed=../vendor/efp/efp_c_api/elastic_frame_protocol_c_api.h");
    println!("cargo:rerun-if-changed=efp-cmake/CMakeLists.txt");

    // Build the EFP static library using our wrapper CMakeLists.txt
    let dst = cmake::build("efp-cmake");

    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=efp");

    // Link C++ standard library
    let target = env::var("TARGET").unwrap();
    if target.contains("apple") || target.contains("freebsd") {
        println!("cargo:rustc-link-lib=c++");
    } else if target.contains("linux") {
        println!("cargo:rustc-link-lib=stdc++");
    } else if target.contains("windows") {
        // MSVC links C++ runtime automatically
    }

    // Generate bindings from the C API header
    let efp_source = PathBuf::from("../vendor/efp");
    let header = efp_source
        .join("efp_c_api")
        .join("elastic_frame_protocol_c_api.h");

    let bindings = bindgen::Builder::default()
        .header(header.to_str().unwrap())
        .allowlist_function("efp_.*")
        .allowlist_type("efp_.*")
        .allowlist_var("EFP_.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
