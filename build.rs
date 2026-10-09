fn main() {
    // The nvim_* C API symbols are resolved by Neovim when it dlopens the
    // module, so the linker must not require them at build time.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
    }
}

