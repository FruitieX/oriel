//! Windows builds embed the icon and version information in oriel.exe
//! (shown by Explorer, the taskbar and the installer).

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest
        .join("../../assets/oriel.ico")
        .canonicalize()
        .unwrap();
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u32> = version
        .split(['.', '-'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    parts.resize(4, 0);
    let numeric = parts
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc = out.join("oriel.rc");
    std::fs::write(
        &rc,
        format!(
            r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "FruitieX"
      VALUE "FileDescription", "oriel remote desktop"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "oriel"
      VALUE "OriginalFilename", "oriel.exe"
      VALUE "ProductName", "oriel"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
            icon = icon.display().to_string().replace('\\', "/"),
        ),
    )
    .unwrap();
    let windres = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        format!(
            "{}-w64-mingw32-windres",
            env::var("CARGO_CFG_TARGET_ARCH").unwrap()
        )
    } else {
        "windres".to_string()
    };
    let object = out.join("oriel-res.o");
    let status = Command::new(&windres)
        .arg(&rc)
        .args(["-O", "coff", "-o"])
        .arg(&object)
        .status()
        .unwrap_or_else(|e| panic!("run {windres}: {e}"));
    assert!(status.success(), "{windres} failed");
    println!("cargo:rustc-link-arg-bins={}", object.display());
}
