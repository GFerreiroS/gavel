fn main() {
    // Surfaced in the boot banner so the log says which target actually ran.
    println!(
        "cargo:rustc-env=TARGET_TRIPLE={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!("cargo:rerun-if-changed=build.rs");
}
