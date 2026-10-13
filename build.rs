use std::{env, path::PathBuf};

use cc::Build;

fn main() {
    let mut build = Build::new();

    for dir in ffmpeg_include_dirs() {
        build.include(dir);
    }

    let build = build
        .file("src/error.c")
        .file("src/logger.c")
        .file("src/packet.c")
        .file("src/time.c")
        .file("src/format/demuxer.c")
        .file("src/format/io.c")
        .file("src/format/muxer.c")
        .file("src/format/stream.c")
        .file("src/codec/bsf.c")
        .file("src/codec/mod.c")
        .file("src/codec/frame.c")
        .file("src/codec/audio/resampler.c")
        .file("src/codec/video/scaler.c");

    // the target's OS, not the build script's (`cfg!` here is the host's): iOS has VideoToolbox too
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let apple = target_os == "macos" || target_os == "ios";

    if apple {
        build.file("src/codec/video/videotoolbox.c");
    }
    if target_os == "android" {
        build.file("src/codec/video/mediacodec.c");
    }

    build.compile("ffwrapper");

    println!("cargo:rerun-if-changed=src");

    link_static("ffwrapper");

    for dir in ffmpeg_lib_dirs() {
        println!("cargo:rustc-link-search=native={}", dir.to_str().unwrap());
    }

    let ffmpeg_link_mode = lib_mode("ffmpeg");

    link("avcodec", ffmpeg_link_mode);
    link("avformat", ffmpeg_link_mode);
    link("avutil", ffmpeg_link_mode);
    link("swresample", ffmpeg_link_mode);
    link("swscale", ffmpeg_link_mode);
    
    // Link VideoToolbox framework on macOS and iOS
    if apple {
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
    }
}

/// `<name>_<target triple>` (`FFMPEG_LIB_DIR_aarch64_apple_darwin`) if it's set, else `<name>`,
/// as the cc crate does: one cargo run that builds several targets
/// (tools/package-macos.sh's universal build) gives each its own FFmpeg.
fn target_env_var(name: &str) -> Result<String, env::VarError> {
    let target = env::var("TARGET").unwrap().replace('-', "_");
    let target_name = format!("{name}_{target}");
    println!("cargo:rerun-if-env-changed={target_name}");
    println!("cargo:rerun-if-env-changed={name}");
    env::var(target_name).or_else(|_| env::var(name))
}

fn ffmpeg_include_dirs() -> Vec<PathBuf> {
    if let Ok(dir) = target_env_var("FFMPEG_INCLUDE_DIR") {
        let dir = PathBuf::from(dir);

        if dir.is_dir() {
            return vec![dir];
        }
    }

    panic!("FFMPEG_INCLUDE_DIR is not set, or isn't a directory");

    /*
    let lib = Config::new()
        .cargo_metadata(false)
        .env_metadata(false)
        .print_system_libs(false)
        .print_system_cflags(false)
        .probe("libavcodec")
        .expect("Unable to find FFmpeg include dir. You can specify it explicitly by setting the FFMPEG_INCLUDE_DIR environment variable.");

    lib.include_paths
    */
}

fn ffmpeg_lib_dirs() -> Vec<PathBuf> {
    if let Ok(dir) = target_env_var("FFMPEG_LIB_DIR") {
        let dir = PathBuf::from(dir);

        if dir.is_dir() {
            return vec![dir];
        }
    }

    panic!("FFMPEG_LIB_DIR is not set, or isn't a directory");

    /*
    let lib = Config::new()
        .cargo_metadata(false)
        .env_metadata(false)
        .print_system_libs(false)
        .print_system_cflags(false)
        .probe("libavcodec")
        .expect("Unable to find FFmpeg lib dir. You can specify it explicitly by setting the FFMPEG_LIB_DIR environment variable.");

    lib.link_paths
    */
}

fn link_static(lib: &str) {
    link(lib, "static")
}

fn link(lib: &str, mode: &str) {
    println!("cargo:rustc-link-lib={}={}", mode, lib);
}

fn lib_mode(lib: &str) -> &'static str {
    let kind = env::var(&format!("{}_STATIC", lib.to_uppercase()));

    match kind.ok().as_deref() {
        Some("0") => "dylib",
        Some(_) => "static",
        None => "dylib",
    }
}
