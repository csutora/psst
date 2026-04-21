fn main() {
    // Embed Info.plist into the Mach-O binary so macOS recognizes our bundle identity.
    // UNUserNotificationCenter requires a valid CFBundleIdentifier to function.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let plist_path = format!("{manifest_dir}/Info.plist");

        println!("cargo:rustc-link-arg=-sectcreate");
        println!("cargo:rustc-link-arg=__TEXT");
        println!("cargo:rustc-link-arg=__info_plist");
        println!("cargo:rustc-link-arg={plist_path}");
        println!("cargo:rerun-if-changed=Info.plist");
    }
}
