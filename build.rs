use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=resources/");
    println!("cargo:rerun-if-changed=assets/");

    // 编译 GResource → 二进制文件
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let gresource = format!("{}/icons.gresource", out_dir);

    let status = Command::new("glib-compile-resources")
        .args([
            "--sourcedir",
            "resources",
            "--target",
            &gresource,
            "resources/icons.gresource.xml",
        ])
        .status()
        .expect("failed to run glib-compile-resources");
    assert!(status.success(), "glib-compile-resources failed");
}
