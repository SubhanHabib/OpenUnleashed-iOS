//! **PLATFORM GLUE** (packaging): a Windows build gets the application icon as a resource of the executable
//! (`res/app.rc`); cross-compiling, `embed-resource` compiles it with `llvm-rc`. Other targets embed nothing.
//!
//! iOS: SDL's Objective-C (`@available` checks) calls `__isPlatformVersionAtLeast`, which lives in clang's own runtime
//! library; rustc links with `-nodefaultlibs`, so that library is named here.

fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=../../packaging/brand/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("res/app.rc", embed_resource::NONE).manifest_required().unwrap();
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        let sim = std::env::var("TARGET").is_ok_and(|t| t.ends_with("-sim"));
        let sdk = if sim { "iphonesimulator" } else { "iphoneos" };
        let out = std::process::Command::new("xcrun").args(["-sdk", sdk, "clang", "--print-resource-dir"]).output();
        if let Ok(o) = out
            && o.status.success()
        {
            let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
            println!("cargo:rustc-link-search=native={dir}/lib/darwin");
            println!("cargo:rustc-link-lib=static=clang_rt.{}", if sim { "iossim" } else { "ios" });
        }
    }
}
