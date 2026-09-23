use std::{env, path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let noos = root.join("third_party/no-OS");
    let inc = ["ad9361", "axi_core", "platform", "include", "."].map(|d| noos.join(d));
    let csrc = PathBuf::from("csrc");

    let mut b = cc::Build::new();
    b.warnings(false).opt_level(2).include(&csrc).define("LINUX_PLATFORM", None);
    for i in &inc {
        b.include(i);
    }
    for f in [
        "ad9361/ad9361.c", "ad9361/ad9361_api.c", "ad9361/ad9361_util.c", "ad9361/ad9361_conv.c", "ad9361/ad9361_ext_band_ctrl.c",
        "axi_core/axi_adc_core.c", "axi_core/axi_dac_core.c",
        "platform/linux_spi.c", "platform/linux_delay.c",
        "api/no_os_spi.c", "api/no_os_gpio.c",
        "util/no_os_util.c", "util/no_os_alloc.c", "util/no_os_mutex.c",
    ] {
        b.file(noos.join(f));
    }
    b.file(csrc.join("shim.c")).compile("noos");

    // bindgen parses with libclang, which needs to be told where a libc for
    // the *target* lives.  zig ships musl's headers; borrow them.
    let zig_lib = String::from_utf8(Command::new("zig").arg("env").output().expect("zig").stdout).unwrap();
    let zig_lib = zig_lib
        .lines()
        .find_map(|l| l.split_once("lib_dir").map(|(_, r)| r.trim_matches(|c: char| !(c.is_alphanumeric() || "/._-".contains(c))).to_string()))
        .expect("zig env: lib_dir");
    let mut bg = bindgen::Builder::default()
        .header("csrc/wrapper.h")
        .clang_args(["--target=armv7-unknown-linux-musleabihf", "-nostdinc"])
        .clang_arg(format!("-I{zig_lib}/include"))
        .clang_arg(format!("-I{zig_lib}/libc/include/arm-linux-musl"))
        .clang_arg(format!("-I{zig_lib}/libc/include/generic-musl"))
        .clang_arg(format!("-I{zig_lib}/libc/include/any-linux-any"))
        .clang_arg("-Icsrc")
        .allowlist_function("ad9361_.*|axi_(adc|dac)_.*|pmp_.*")
        .allowlist_type("AD9361_.*|ad9361_.*|no_os_gpio.*|axi_.*|gpio_v2_.*")
        .allowlist_var("ID_AD936.*|ENSM_.*|RF_GAIN_.*|AXI_DAC_.*|NO_OS_GPIO.*|GPIO_V2_.*")
        .derive_default(true)
        .layout_tests(false);
    for i in &inc {
        bg = bg.clang_arg(format!("-I{}", i.display()));
    }
    bg.generate()
        .expect("bindgen")
        .write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("noos.rs"))
        .unwrap();

    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=build.rs");
}
