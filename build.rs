fn main() {
    println!("cargo:rustc-link-arg-bins=-Tlinkall.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    // println!("cargo:rustc-link-arg-bins=force-frame-pointers");
    // println!("cargo:rustc-link-arg-bins=-Tesp32c6_rom_functions.x");
}
