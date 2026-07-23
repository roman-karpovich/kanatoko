use std::env;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(kanatoko_protocol_27_fixtures)");

    if env::var("CARGO_PKG_VERSION_MAJOR").as_deref() == Ok("27") {
        println!("cargo:rustc-cfg=kanatoko_protocol_27_fixtures");
    }
}
