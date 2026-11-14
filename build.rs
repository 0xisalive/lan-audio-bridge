//! Build script. On FreeBSD it compiles the small OSS ioctl shim (src/oss_shim.c) and
//! links it into the binary. The shim expands the SNDCTL_DSP_* request macros against
//! the system <sys/soundcard.h>, so the Rust code never hardcodes ioctl numbers.
//!
//! On every other platform this is a no-op; the OSS backend is compiled out.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "freebsd" {
        println!("cargo:rerun-if-changed=src/oss_shim.c");
        cc::Build::new().file("src/oss_shim.c").compile("oss_shim");
    }
}
