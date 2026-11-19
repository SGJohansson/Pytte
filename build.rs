//! Bäddar in programikonen (res/pytte.ico, resurs-id 1) och versionsinfo i exe:n.
//! Resurskompilatorn (rc.exe) finns bara på Windows-värdar; vid korskontroll från
//! andra system byggs exe:n utan ikon och fönstret faller tillbaka på standardikonen.

fn main() {
    println!("cargo:rerun-if-changed=res/pytte.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if cfg!(windows) && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("res/pytte.ico")
            .compile()
            .expect("kunde inte bädda in ikonen (saknas rc.exe från Windows SDK?)");
    }
}
