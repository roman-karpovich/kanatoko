use std::env;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(kanatoko_protocol_27_fixtures)");
    println!("cargo:rustc-check-cfg=cfg(kanatoko_protocol_28_fixtures)");
    println!("cargo:rustc-check-cfg=cfg(kanatoko_local_wasm_fixtures)");

    match env::var("CARGO_PKG_VERSION_MAJOR").as_deref() {
        Ok("27") => {
            println!("cargo:rustc-cfg=kanatoko_protocol_27_fixtures");
            println!("cargo:rustc-cfg=kanatoko_local_wasm_fixtures");
        }
        Ok("28") => {
            println!("cargo:rustc-cfg=kanatoko_protocol_28_fixtures");
            println!("cargo:rustc-cfg=kanatoko_local_wasm_fixtures");
        }
        _ => {}
    }
}
