fn main() {
    // intel_tex_2 compiles a C++ source (ASTC helpers) that needs the C++
    // runtime on Unix; MSVC links it implicitly.
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => println!("cargo:rustc-link-lib=stdc++"),
        Ok("macos") => println!("cargo:rustc-link-lib=c++"),
        _ => {}
    }
}
