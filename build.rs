use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src/directwrite.cpp");
    cc::Build::new()
        .cpp(true)
        .flag("/utf-8")
        .flag("/EHsc")
        .file("src/directwrite.cpp")
        .compile("portside_native");
    for lib in ["d2d1", "dwrite", "ole32"] {
        println!("cargo:rustc-link-lib={lib}");
    }

    // The exe's icon and version info.
    println!("cargo:rerun-if-changed=src/portside.rc");
    println!("cargo:rerun-if-changed=assets/portside.ico");
    let res = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("portside.res");
    let version = |part| env::var(format!("CARGO_PKG_VERSION_{part}")).unwrap();
    let status = cc::windows_registry::find(&env::var("TARGET").unwrap(), "rc.exe")
        .expect("the Windows SDK should have rc.exe")
        .arg("/nologo")
        .arg(format!(
            "/dVERSION={},{},{},0",
            version("MAJOR"),
            version("MINOR"),
            version("PATCH")
        ))
        .arg(format!(
            "/dVERSION_TEXT=\"{}\"",
            env::var("CARGO_PKG_VERSION").unwrap()
        ))
        .arg("/fo")
        .arg(&res)
        .arg("src/portside.rc")
        .status()
        .expect("rc.exe should run");
    assert!(status.success(), "rc.exe could not compile src/portside.rc");
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
