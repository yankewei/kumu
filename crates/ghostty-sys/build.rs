//! Builds libghostty-vt from vendor/ghostty with Zig and generates bindings.

use std::path::PathBuf;
use std::process::Command;

fn zig_target(target: &str) -> &'static str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("ghostty-sys: no Zig target for {other}"),
    }
}

fn zig_bin() -> String {
    if let Ok(zig) = std::env::var("ZIG")
        && !zig.is_empty()
    {
        return zig;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/zig");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "zig".to_string()
}

fn static_lib_names() -> (&'static str, &'static str) {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        ("ghostty-vt-static.lib", "ghostty-vt.lib")
    } else {
        ("libghostty-vt.a", "libghostty-vt.a")
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest
        .join("../../vendor/ghostty")
        .canonicalize()
        .expect("vendor/ghostty missing: git submodule update --init");
    let src = PathBuf::from(src.to_string_lossy().trim_start_matches(r"\\?\").to_owned());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let lib = out.join("lib");
    std::fs::create_dir_all(&lib).unwrap();

    println!("cargo:rerun-if-changed={}", src.join("include").display());
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_LIB_DIR");

    let (zig_lib_name, lib_name) = static_lib_names();
    if let Ok(prebuilt_dir) = std::env::var("GHOSTTY_VT_LIB_DIR")
        && !prebuilt_dir.is_empty()
    {
        let prebuilt = PathBuf::from(&prebuilt_dir).join(lib_name);
        assert!(
            prebuilt.is_file(),
            "GHOSTTY_VT_LIB_DIR={prebuilt_dir:?} does not contain {lib_name}"
        );
        println!("cargo:rerun-if-changed={}", prebuilt.display());
        std::fs::copy(&prebuilt, lib.join(lib_name)).unwrap();
    } else {
        for path in ["src", "build.zig", "build.zig.zon"] {
            println!("cargo:rerun-if-changed={}", src.join(path).display());
        }
        println!("cargo:rerun-if-env-changed=ZIG");
        println!("cargo:rerun-if-env-changed=ZIG_LOCAL_CACHE_DIR");

        let zig = zig_bin();
        let mut cmd = Command::new(&zig);
        cmd.current_dir(&src)
            .args([
                "build",
                "-Demit-lib-vt",
                "-Demit-xcframework=false",
                "-Doptimize=ReleaseFast",
            ])
            .arg(format!(
                "-Dtarget={}",
                zig_target(&std::env::var("TARGET").unwrap())
            ))
            .arg("--prefix")
            .arg(out.join("zig-out"));
        if std::env::var_os("ZIG_LOCAL_CACHE_DIR")
            .filter(|v| !v.is_empty())
            .is_none()
        {
            cmd.arg("--cache-dir").arg(out.join("zig-cache"));
        }
        let status = cmd
            .status()
            .unwrap_or_else(|e| panic!("cannot run {zig:?} (Zig 0.17 required): {e}"));
        assert!(
            status.success(),
            "zig build of libghostty-vt failed: {status}"
        );

        std::fs::copy(
            out.join("zig-out/lib").join(zig_lib_name),
            lib.join(lib_name),
        )
        .unwrap();
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");

    let include = src.join("include");
    let mut builder = bindgen::Builder::default()
        .header(include.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", include.display()));
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        if let Ok(sdk) = Command::new("xcrun").args(["--show-sdk-path"]).output()
            && sdk.status.success()
        {
            let path = String::from_utf8_lossy(&sdk.stdout);
            let path = path.trim();
            if !path.is_empty() {
                builder = builder.clang_arg(format!("-isysroot{path}"));
            }
        }
    } else if Command::new("clang").arg("--version").output().is_err() {
        let zig = zig_bin();
        let zig_env = Command::new(&zig).arg("env").output().expect("zig env");
        let zig_env = String::from_utf8_lossy(&zig_env.stdout);
        let zig_lib = zig_env
            .lines()
            .find_map(|line| line.trim().strip_prefix(".lib_dir = \""))
            .and_then(|line| line.strip_suffix("\","))
            .expect("zig env has no lib_dir");
        builder = builder.clang_arg(format!("-isystem{zig_lib}/include"));
    }
    builder
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed on ghostty/vt.h")
        .write_to_file(out.join("bindings.rs"))
        .unwrap();
}
