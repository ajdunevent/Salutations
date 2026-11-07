//! Sets the `camel_3_58` cfg when the installed Camel is 3.58 or newer;
//! see src/compat.rs.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(camel_3_58)");
    let lib = pkg_config::Config::new()
        .cargo_metadata(false)
        .env_metadata(true)
        .probe("camel-1.2")
        .expect("camel-1.2 from pkg-config");
    let mut parts = lib.version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if version >= (3, 58) {
        println!("cargo::rustc-cfg=camel_3_58");
    }
}
