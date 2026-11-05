// Embed Info.plist into the binary's __TEXT,__info_plist section.
// Core Audio taps / microphone access require the usage-description keys to be
// present, otherwise TCC may silently hand us all-zero buffers.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=Info.plist");
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{dir}/Info.plist");
}
