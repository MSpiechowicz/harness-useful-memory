//! Names the version the binary reports. Only the release pipeline sets `HARNESS_MEMORY_RELEASE=1`, so every other
//! build (a source build, a dev branch) reports `<version>-dev` and is never mistaken for a published release.

fn main() {
    println!("cargo:rerun-if-env-changed=HARNESS_MEMORY_RELEASE");
    let version = env!("CARGO_PKG_VERSION");
    let release = std::env::var("HARNESS_MEMORY_RELEASE").is_ok_and(|value| value == "1");
    let reported = if release {
        version.to_owned()
    } else {
        format!("{version}-dev")
    };
    println!("cargo:rustc-env=HARNESS_MEMORY_VERSION={reported}");
}
