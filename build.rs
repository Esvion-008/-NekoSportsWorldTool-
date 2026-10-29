//! Windows 资源嵌入：exe 文件图标 + 版本信息（ProductName/FileDescription 均为定制名称）。
//! 仅依赖 MinGW 自带 windres，不引入额外 crate。

use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let res = format!("{out_dir}/app.res");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // -c 65001：RC 文件为 UTF-8（含中文版本信息）
    let status = Command::new("windres")
        .args(["-c", "65001", "app.rc", "-O", "coff", "-o", &res])
        .status()
        .expect("windres 启动失败（需要 MinGW bin 在 PATH 中）");
    assert!(status.success(), "windres 编译 app.rc 失败");
    println!("cargo:rustc-link-arg={res}");
}
