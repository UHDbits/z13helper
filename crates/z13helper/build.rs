use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let resources = manifest.join("resources");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("z13helper.gresource");

    let status = Command::new("glib-compile-resources")
        .arg(resources.join("z13helper.gresource.xml"))
        .arg(format!("--sourcedir={}", resources.display()))
        .arg(format!("--target={}", output.display()))
        .status()
        .expect("glib-compile-resources is required to build z13helper");
    assert!(status.success(), "failed to compile z13helper resources");

    println!("cargo:rerun-if-changed=resources/z13helper.gresource.xml");
    println!("cargo:rerun-if-changed=resources/z13helper-battery-limit-symbolic.svg");
}
