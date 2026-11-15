fn main() {
    // WinFsp is delay-loaded so binaries start without it and can report a
    // friendly error (design §11). The import library is bundled by winfsp-sys.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=dylib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
    }
}
