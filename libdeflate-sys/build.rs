use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let dst = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    #[cfg(all(feature = "dynamic", not(feature = "nonfinal")))]
    if pkg_config::Config::new()
        .print_system_libs(false)
        .cargo_metadata(true)
        .atleast_version("1.23")
        .probe("libdeflate")
        .is_ok()
    {
        return;
    }
    else {
        println!("cargo:warning=Dynamic linking failed; falling back to static build.");
    }

    let mut build = cc::Build::new();

    build
        .files(&[
            "libdeflate/lib/arm/cpu_features.c",
            "libdeflate/lib/x86/cpu_features.c",
            "libdeflate/lib/adler32.c",
            "libdeflate/lib/crc32.c",
            "libdeflate/lib/deflate_compress.c",
            "libdeflate/lib/deflate_decompress.c",
            "libdeflate/lib/gzip_compress.c",
            "libdeflate/lib/gzip_decompress.c",
            "libdeflate/lib/utils.c",
            "libdeflate/lib/zlib_compress.c",
            "libdeflate/lib/zlib_decompress.c",
        ])
        .include("libdeflate")
        .warnings(false)
        .out_dir(dst.join("lib"));

    // libdeflate 1.25 removed a workaround for compilers that reject its AVX-512 code (e.g.
    // "always_inline function '_mm512_set1_epi8' requires target feature 'evex512'" from the
    // clang versions zig 0.14 and 0.15 ship, cross compiling for x86 Linux). Enabling EVEX512
    // explicitly fixes them, and is harmless (or ignored) elsewhere.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if (target_arch == "x86_64" || target_arch == "x86") && !build.get_compiler().is_like_msvc() {
        build.flag_if_supported("-mevex512");
    }

    if cfg!(feature = "freestanding") && !build.get_compiler().is_like_msvc() {
        build
            .flag("-ffreestanding")
            .flag("-nostdlib")
            .define("FREESTANDING", None);
    }

    build.compile("deflate");

    let src = Path::new("libdeflate");
    let include = dst.join("include");
    fs::create_dir_all(&include).unwrap();
    fs::copy(src.join("libdeflate.h"), include.join("libdeflate.h")).unwrap();
    println!("cargo:root={}", dst.display());
    println!("cargo:include={}", include.display());
}
