use ::tap::*;
use anyhow::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};
static MACOS_SDK_PATH: LazyLock<String> = LazyLock::new(|| {
    String::from_utf8(
        std::process::Command::new("xcrun")
            .arg("--show-sdk-path")
            .output()
            .expect("Failed to get macOS SDK path")
            .stdout,
    )
    .expect("Invalid UTF-8 from xcrun")
    .trim()
    .to_string()
});
const VENDOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/vendor");
const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
static TARGET_OS: LazyLock<String> =
    LazyLock::new(|| std::env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set"));
static TARGET_ARCH: LazyLock<String> = LazyLock::new(|| {
    std::env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH not found")
});
static EMSCRIPTEN_CACHE: LazyLock<String> = LazyLock::new(|| {
    let emscripten_cache = std::process::Command::new("em-config")
        .arg("CACHE")
        .output()
        .expect("Failed to get emscripten cache")
        .stdout;
    let emscripten_cache = std::str::from_utf8(&emscripten_cache)
        .expect("Failed to parse emscripten cache")
        .trim()
        .to_string();
    emscripten_cache
});

static MNN_COMPILE: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("MNN_COMPILE")
        .ok()
        .and_then(|v| match v.as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        })
        .unwrap_or(true)
});

const HALIDE_SEARCH: &str =
    r#"HALIDE_ATTRIBUTE_ALIGN(1) halide_type_code_t code; // halide_type_code_t"#;
const TRACING_SEARCH: &str = "#define MNN_PRINT(format, ...) printf(format, ##__VA_ARGS__)\n#define MNN_ERROR(format, ...) printf(format, ##__VA_ARGS__)";
const TRACING_REPLACE: &str = r#"
enum class Level {
  Info = 0,
  Error = 1,
};
extern "C" {
void mnn_ffi_emit(const char *file, size_t line, Level level,
                  const char *message);
}
#define MNN_PRINT(format, ...)                                                 \
  {                                                                            \
    char logtmp[4096];                                                         \
    snprintf(logtmp, 4096, format, ##__VA_ARGS__);                             \
    mnn_ffi_emit(__FILE__, __LINE__, Level::Info, logtmp);                     \
  }

#define MNN_ERROR(format, ...)                                                 \
  {                                                                            \
    char logtmp[4096];                                                         \
    snprintf(logtmp, 4096, format, ##__VA_ARGS__);                             \
    mnn_ffi_emit(__FILE__, __LINE__, Level::Error, logtmp);                    \
  }
"#;

fn ensure_vendor_exists(vendor: impl AsRef<Path>) -> Result<()> {
    if vendor
        .as_ref()
        .read_dir()
        .with_context(|| format!("Vendor directory missing: {}", vendor.as_ref().display()))?
        .flatten()
        .count()
        == 0
    {
        anyhow::bail!("Vendor not found maybe you need to run \"git submodule update --init\"")
    }
    Ok(())
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MNN_SRC");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let source = PathBuf::from(
        std::env::var("MNN_SRC")
            .ok()
            .unwrap_or_else(|| VENDOR.into()),
    );

    ensure_vendor_exists(&source)?;

    let vendor = out_dir.join("vendor");
    // std::fs::remove_dir_all(&vendor).ok();
    if !vendor.exists() {
        fs_extra::dir::copy(
            &source,
            &vendor,
            &fs_extra::dir::CopyOptions::new()
                .overwrite(true)
                .copy_inside(true),
        )
        .context("Failed to copy vendor")?;
        let intptr = vendor.join("include").join("MNN").join("HalideRuntime.h");
        #[cfg(unix)]
        std::fs::set_permissions(&intptr, std::fs::Permissions::from_mode(0o644))?;

        use itertools::Itertools;
        let intptr_contents = std::fs::read_to_string(&intptr)?;
        let patched = intptr_contents.lines().collect::<Vec<_>>();
        if let Some((idx, _)) = patched
            .iter()
            .find_position(|line| line.contains(HALIDE_SEARCH))
        {
            // remove the last line and the next 3 lines
            let patched = patched
                .into_iter()
                .enumerate()
                .filter(|(c_idx, _)| !(*c_idx == idx - 1 || (idx + 1..=idx + 3).contains(c_idx)))
                .map(|(_, c)| c)
                .collect::<Vec<_>>();

            std::fs::write(intptr, patched.join("\n"))?;
        }

        let mnn_define = vendor.join("include").join("MNN").join("MNNDefine.h");
        let patched =
            std::fs::read_to_string(&mnn_define)?.replace(TRACING_SEARCH, TRACING_REPLACE);
        #[cfg(unix)]
        std::fs::set_permissions(&mnn_define, std::fs::Permissions::from_mode(0o644))?;
        std::fs::write(mnn_define, patched)?;
    }

    if *MNN_COMPILE {
        let install_dir = out_dir.join("mnn-install");
        build_cmake(&vendor, &install_dir)?;
        println!(
            "cargo:rustc-link-search=native={}",
            install_dir.join("lib").display()
        );
    } else if let core::result::Result::Ok(lib_dir) = std::env::var("MNN_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", lib_dir);
    } else {
        panic!("MNN_LIB_DIR not set while MNN_COMPILE is false");
    }

    mnn_c_build(PathBuf::from(MANIFEST_DIR).join("mnn_c"), &vendor)
        .with_context(|| "Failed to build mnn_c")?;
    mnn_c_bindgen(&vendor, &out_dir).with_context(|| "Failed to generate mnn_c bindings")?;
    mnn_cpp_bindgen(&vendor, &out_dir).with_context(|| "Failed to generate mnn_cpp bindings")?;
    println!("cargo:include={vendor}/include", vendor = vendor.display());
    if *TARGET_OS == "macos" {
        #[cfg(feature = "metal")]
        println!("cargo:rustc-link-lib=framework=Foundation");
        #[cfg(feature = "metal")]
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        #[cfg(feature = "metal")]
        println!("cargo:rustc-link-lib=framework=Metal");
        #[cfg(feature = "coreml")]
        println!("cargo:rustc-link-lib=framework=CoreML");
        #[cfg(feature = "coreml")]
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        #[cfg(feature = "opencl")]
        println!("cargo:rustc-link-lib=framework=OpenCL");
        #[cfg(feature = "opengl")]
        println!("cargo:rustc-link-lib=framework=OpenGL");
    } else {
        // #[cfg(feature = "opencl")]
        // println!("cargo:rustc-link-lib=static=opencl");
    }
    if is_emscripten() {
        // println!("cargo:rustc-link-lib=static=stdc++");
        let emscripten_cache = std::process::Command::new("em-config")
            .arg("CACHE")
            .output()?
            .stdout;
        let emscripten_cache = std::str::from_utf8(&emscripten_cache)?.trim();
        let wasm32_emscripten_libs =
            PathBuf::from(emscripten_cache).join("sysroot/lib/wasm32-emscripten");
        println!(
            "cargo:rustc-link-search=native={}",
            wasm32_emscripten_libs.display()
        );
    }
    println!("cargo:rustc-link-lib=static=MNN");
    Ok(())
}

static IS_MSVC_TARGET: LazyLock<bool> = LazyLock::new(|| {
    *TARGET_OS == "windows"
        && *TARGET_ARCH == "x86_64"
        && std::env::consts::OS != "windows" // Ensure we are cross-compiling
});

// ... (other functions)

pub fn mnn_c_bindgen(vendor: impl AsRef<Path>, out: impl AsRef<Path>) -> Result<()> {
    let vendor = vendor.as_ref();
    let mnn_c = PathBuf::from(MANIFEST_DIR).join("mnn_c");
    mnn_c.read_dir()?.flatten().for_each(|e| {
        rerun_if_changed(e.path());
    });
    const HEADERS: &[&str] = &[
        "error_code_c.h",
        "interpreter_c.h",
        "tensor_c.h",
        "backend_c.h",
        "schedule_c.h",
    ];

    let mut builder = bindgen::Builder::default()
        .clang_args(["-x", "c++"]) // Treat headers as C++
        .clang_args(["-std=c++14"]) // Use C++14 standard
        .clang_arg(CxxOption::VULKAN.cxx())
        .clang_arg(CxxOption::METAL.cxx())
        .clang_arg(CxxOption::COREML.cxx())
        .clang_arg(CxxOption::OPENCL.cxx())
        .clang_arg("-D__STDC_LIMIT_MACROS");

    // Only add macOS-specific flags when targeting macOS
    if *TARGET_OS == "macos" {
        builder = builder
            .clang_arg("-D__APPLE__")
            .clang_arg(format!("-isysroot{}", *MACOS_SDK_PATH));
    }

    if is_emscripten() {
        println!("cargo:rustc-cdylib-link-arg=-fvisibility=default");
        builder = builder
            .clang_arg("-fvisibility=default")
            .clang_arg("--target=wasm32-emscripten")
            .clang_arg(format!("-I{}/sysroot/include", emscripten_cache()));
    } else if *IS_MSVC_TARGET {
        // When cross-compiling to MSVC from non-Windows, rely on cargo-xwin to set up the environment
        // Clang will pick up INCLUDE, LIB, and PATH environment variables set by cargo-xwin.
        builder = builder.clang_arg("--target=x86_64-pc-windows-msvc");
        // No explicit -I or --sysroot needed here if cargo-xwin has properly configured the environment for clang.
        // We'll add a check/guidance if the environment isn't set.
        if std::env::var("INCLUDE").is_err() || std::env::var("LIB").is_err() {
            println!("cargo:warning=When cross-compiling to x86_64-pc-windows-msvc from macOS/Linux, ensure you are using `cargo xwin build` or have correctly set `INCLUDE` and `LIB` environment variables pointing to the Windows SDK and MSVC toolchain.");
        }
    }
        
    let bindings = builder
        .clang_arg(format!("-I{}", vendor.join("include").to_string_lossy()))
        .pipe(|generator| {
            HEADERS.iter().fold(generator, |gen, header| {
                gen.header(mnn_c.join(header).to_string_lossy())
            })
        })
        .newtype_enum("MemoryMode")
        .newtype_enum("PowerMode")
        .newtype_enum("PrecisionMode")
        .constified_enum_module("SessionMode")
        .rustified_enum("DimensionType")
        .rustified_enum("HandleDataType")
        .rustified_enum("MapType")
        .rustified_enum("halide_type_code_t")
        .rustified_enum("ErrorCode")
        .newtype_enum("MNNGpuMode")
        .newtype_enum("MNNForwardType")
        .newtype_enum("RuntimeStatus")
        .no_copy("CString")
        .generate_cstr(true)
        .generate_inline_functions(false)
        .size_t_is_usize(true)
        .emit_diagnostics()
        .detect_include_paths(std::env::var("TARGET") == std::env::var("HOST"))
        .ctypes_prefix("core::ffi")
        // .tap(|d| {
        //     // eprintln!("Full bindgen: {}", d.command_line_flags().join(" "));
        //     std::fs::write("bindgen.txt", d.command_line_flags().join(" ")).ok();
        // })
        .generate()?;
    bindings.write_to_file(out.as_ref().join("mnn_c.rs"))?;
    Ok(())
}

pub fn mnn_cpp_bindgen(vendor: impl AsRef<Path>, out: impl AsRef<Path>) -> Result<()> {
    let vendor = vendor.as_ref();
    let mut builder = bindgen::Builder::default()
        .clang_args(["-x", "c++"])
        .clang_args(["-std=c++14"])
        .clang_arg(CxxOption::VULKAN.cxx())
        .clang_arg(CxxOption::METAL.cxx())
        .clang_arg(CxxOption::COREML.cxx())
        .clang_arg(CxxOption::OPENCL.cxx())
        .clang_arg("-D__STDC_LIMIT_MACROS")
        .clang_arg(format!("-I{}", vendor.join("include").to_string_lossy()))
        .generate_cstr(true)
        .generate_inline_functions(false)
        .size_t_is_usize(true)
        .emit_diagnostics()
        .ctypes_prefix("core::ffi")
        .header(
            vendor
                .join("include")
                .join("MNN")
                .join("Interpreter.hpp")
                .to_string_lossy(),
        )
        .allowlist_item(".*SessionInfoCode.*");

    // Only add macOS-specific flags when targeting macOS
    if *TARGET_OS == "macos" {
        builder = builder
            .clang_arg("-D__APPLE__")
            .clang_arg(format!("-isysroot{}", *MACOS_SDK_PATH));
    }

    if *IS_MSVC_TARGET {
        // When cross-compiling to MSVC from non-Windows, rely on cargo-xwin to set up the environment
        // Clang will pick up INCLUDE, LIB, and PATH environment variables set by cargo-xwin.
        builder = builder.clang_arg("--target=x86_64-pc-windows-msvc");
        // No explicit -I or --sysroot needed here if cargo-xwin has properly configured the environment for clang.
        // We'll add a check/guidance if the environment isn't set.
        if std::env::var("INCLUDE").is_err() || std::env::var("LIB").is_err() {
            println!("cargo:warning=When cross-compiling to x86_64-pc-windows-msvc from macOS/Linux, ensure you are using `cargo xwin build` or have correctly set `INCLUDE` and `LIB` environment variables pointing to the Windows SDK and MSVC toolchain.");
        }
    }

    let bindings = builder.generate()?;
    // let cmd = bindings.command_line_flags().join(" ");
    // println!("cargo:warn=bindgen: {}", cmd);
    bindings.write_to_file(out.as_ref().join("mnn_cpp.rs"))?;
    Ok(())
}

pub fn mnn_c_build(path: impl AsRef<Path>, vendor: impl AsRef<Path>) -> Result<()> {
    let mnn_c = path.as_ref();
    let files = mnn_c.read_dir()?.flatten().map(|e| e.path()).filter(|e| {
        e.extension() == Some(std::ffi::OsStr::new("cpp"))
            || e.extension() == Some(std::ffi::OsStr::new("c"))
    });
    let vendor = vendor.as_ref();

    // Special handling for Windows cross-compilation on macOS/Linux
    if *IS_MSVC_TARGET {
        let cc_env = std::env::var("CC_x86_64_pc_windows_msvc").or_else(|_| std::env::var("CC")).unwrap_or_default();
        let is_clang_cl = cc_env.contains("clang-cl");

        if !is_clang_cl {
            anyhow::bail!("Building for x86_64-pc-windows-msvc on macOS/Linux requires `clang-cl` (typically provided by `cargo-xwin`).\n\
                           Please install `cargo-xwin` (`cargo install cargo-xwin`) and then run your build command with `cargo xwin build ...`.\n\
                           Ensure that the `INCLUDE` and `LIB` environment variables are correctly set for the Windows SDK and MSVC toolchain.");
        }
    }

    cc::Build::new()
        .include(vendor.join("include"))
        // .includes(vulkan_includes(vendor))
        .pipe(|config| {
            #[cfg(feature = "vulkan")]
            config.define("MNN_VULKAN", "1");
            #[cfg(feature = "opengl")]
            config.define("MNN_OPENGL", "1");
            #[cfg(feature = "metal")]
            config.define("MNN_METAL", "1");
            #[cfg(feature = "coreml")]
            config.define("MNN_COREML", "1");
            #[cfg(feature = "opencl")]
            config.define("MNN_OPENCL", "ON");
            if is_emscripten() {
                config.compiler("emcc");
                // We can't compile wasm32-unknown-unknown with emscripten
                config.target("wasm32-unknown-emscripten");
                config.cpp_link_stdlib("c++-noexcept");
            }
            #[cfg(feature = "crt_static")]
            config.static_crt(true);

            // No bail logic here now, just configure config
            config
        })
        .cpp(true)
        .files(files)
        .std("c++14")
        // .pipe(|build| {
        //     let c = build.get_compiler();
        //     use std::io::Write;
        //     writeln!(
        //         std::fs::File::create("./command.txt").unwrap(),
        //         "{:?}",
        //         c.to_command()
        //     )
        //     .unwrap();
        //     build
        // })
        .try_compile("mnn_c")
        .context("Failed to compile mnn_c library")?;
    Ok(())
}

pub fn build_cmake(path: impl AsRef<Path>, install: impl AsRef<Path>) -> Result<()> {
    let threads = std::thread::available_parallelism()?;

    // Special handling for Windows MSVC cross-compilation on macOS/Linux
    // We manually run cmake to avoid cmake-rs injecting incompatible flags (like -A x64 with Unix Makefiles)
    if *TARGET_OS == "windows" && *TARGET_ARCH == "x86_64" && std::env::consts::OS != "windows" {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
        let build_dir = out_dir.join("build-mnn-manual");
        
        // Force clean build directory to avoid cache pollution
        if build_dir.exists() {
            std::fs::remove_dir_all(&build_dir)?;
        }
        std::fs::create_dir_all(&build_dir)?;
        
        let install_str = install.as_ref().to_string_lossy();
        let path_str = path.as_ref().to_string_lossy();

        // Detect compiler from environment variables
        let target_env = "x86_64_pc_windows_msvc";
        let cc_env = format!("CC_{}", target_env);
        let cxx_env = format!("CXX_{}", target_env);

        let cc = std::env::var(&cc_env).or_else(|_| std::env::var("CC")).unwrap_or_default();
        let cxx = std::env::var(&cxx_env).or_else(|_| std::env::var("CXX")).unwrap_or_default();
        let is_clang_cl = cc.contains("clang-cl");

        let mut cmd = std::process::Command::new("cmake");
        cmd.current_dir(&build_dir)
           .arg(&*path_str)
           .arg("-G").arg("Unix Makefiles")
           .arg("-DCMAKE_CXX_STANDARD=14")
           .arg("-DMNN_BUILD_SHARED_LIBS=OFF")
           .arg("-DMNN_SEP_BUILD=OFF")
           .arg("-DMNN_PORTABLE_BUILD=ON")
           .arg("-DMNN_USE_SYSTEM_LIB=OFF")
           .arg("-DMNN_BUILD_CONVERTER=OFF")
           .arg("-DMNN_BUILD_TOOLS=OFF")
           .arg(format!("-DCMAKE_INSTALL_PREFIX={}", install_str))
           .arg("-DCMAKE_SYSTEM_NAME=Windows")
           .arg("-DCMAKE_BUILD_TYPE=Release");

        // Set CMAKE_SYSTEM_PROCESSOR based on TARGET_ARCH
        if *TARGET_ARCH == "x86_64" {
            cmd.arg("-DCMAKE_SYSTEM_PROCESSOR=AMD64");
        } else if *TARGET_ARCH == "aarch64" {
            cmd.arg("-DCMAKE_SYSTEM_PROCESSOR=ARM64");
        } else if *TARGET_ARCH == "x86" {
            cmd.arg("-DCMAKE_SYSTEM_PROCESSOR=X86");
        }

        if !is_clang_cl {
            anyhow::bail!("Building for x86_64-pc-windows-msvc on macOS/Linux requires `clang-cl` (typically provided by `cargo-xwin`).\n\
                           Please install `cargo-xwin` (`cargo install cargo-xwin`) and then run your build command with `cargo xwin build ...`.\n\
                           Ensure that the `INCLUDE` and `LIB` environment variables are correctly set for the Windows SDK and MSVC toolchain.");
        }
        // Configure for clang-cl (MSVC simulation)
        // Use exact target triple for clang-cl
        let target_flag = "--target=x86_64-pc-windows-msvc";

        // Get existing flags from cargo-xwin (which include sysroot paths)
        let env_c_flags = std::env::var("CFLAGS").unwrap_or_default();
        let env_cxx_flags = std::env::var("CXXFLAGS").unwrap_or_default();

        // Explicitly add include paths for cargo-xwin's installed headers
        let xwin_base_path = PathBuf::from(std::env::var("XWIN_CACHE_DIR").unwrap_or_else(|_| {
            // Fallback for default cargo-xwin cache location on macOS
            let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home_dir).join("Library/Caches/cargo-xwin/xwin").to_string_lossy().to_string()
        }));
        let crt_include = xwin_base_path.join("crt/include");
        let sdk_include_base = xwin_base_path.join("sdk/include/10.0.26100"); // Hardcode SDK version for now

        let sdk_ucrt_include = sdk_include_base.join("ucrt");
        let sdk_um_include = sdk_include_base.join("um");
        let sdk_shared_include = sdk_include_base.join("shared");

        let mut extra_c_includes = String::new();
        let mut extra_cxx_includes = String::new();

        if crt_include.exists() {
            extra_c_includes.push_str(&format!("/I{} ", crt_include.to_string_lossy()));
            extra_cxx_includes.push_str(&format!("/I{} ", crt_include.to_string_lossy()));
        }
        if sdk_ucrt_include.exists() {
            extra_c_includes.push_str(&format!("/I{} ", sdk_ucrt_include.to_string_lossy()));
            extra_cxx_includes.push_str(&format!("/I{} ", sdk_ucrt_include.to_string_lossy()));
        }
        if sdk_um_include.exists() {
            extra_c_includes.push_str(&format!("/I{} ", sdk_um_include.to_string_lossy()));
            extra_cxx_includes.push_str(&format!("/I{} ", sdk_um_include.to_string_lossy()));
        }
        if sdk_shared_include.exists() {
            extra_c_includes.push_str(&format!("/I{} ", sdk_shared_include.to_string_lossy()));
            extra_cxx_includes.push_str(&format!("/I{} ", sdk_shared_include.to_string_lossy()));
        }


        let c_flags = format!("{} {} {} -DWIN32=1 /EHsc -msse4.1", env_c_flags, extra_c_includes, target_flag);
        let cxx_flags = format!("{} {} {} -DWIN32=1 /EHsc -msse4.1", env_cxx_flags, extra_cxx_includes, target_flag);

        cmd.arg(format!("-DCMAKE_C_COMPILER={}", cc))
            .arg(format!("-DCMAKE_CXX_COMPILER={}", cxx))
            .arg("-DCMAKE_RC_COMPILER=llvm-rc")
            .arg("-DCMAKE_MT=llvm-mt")
            .arg("-DCMAKE_LINKER=lld-link")
            // Force Release configuration for compiler checks to avoid looking for msvcrtd.lib (Debug CRT)
            .arg("-DCMAKE_TRY_COMPILE_CONFIGURATION=Release")
            // Explicitly set runtime library to MultiThreadedDLL (/MD) to match Rust's default
            .arg("-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL")
            // Pass target flags to ensure clang-cl compiles for the correct target, not host
            .arg(format!("-DCMAKE_C_FLAGS={}", c_flags))
            .arg(format!("-DCMAKE_CXX_FLAGS={}", cxx_flags));

        // Don't clear env vars for clang-cl, cargo-xwin needs them

        
        cmd.arg(format!("-DMNN_WIN_RUNTIME_MT={}", CxxOption::CRT_STATIC.cmake_value()))
           .arg(format!("-DMNN_USE_THREAD_POOL={}", CxxOption::THREADPOOL.cmake_value()))
           .arg(format!("-DMNN_OPENMP={}", CxxOption::OPENMP.cmake_value()))
           .arg(format!("-DMNN_VULKAN={}", CxxOption::VULKAN.cmake_value()))
           .arg(format!("-DMNN_METAL={}", CxxOption::METAL.cmake_value()))
           .arg(format!("-DMNN_COREML={}", CxxOption::COREML.cmake_value()))
           .arg(format!("-DMNN_OPENCL={}", CxxOption::OPENCL.cmake_value()))
           .arg(format!("-DMNN_OPENGL={}", CxxOption::OPENGL.cmake_value()))
           .arg("-DMNN_USE_SSE=OFF");
           
        // if *TARGET_OS == "windows" {
        //    cmd.arg("-DCMAKE_CXX_FLAGS=-DWIN32=1");
        // }

        println!("Running manual cmake config: {:?}", cmd);
        let status = cmd.status()?;
        if !status.success() {
            anyhow::bail!("Manual CMake configuration failed");
        }

        let mut build_cmd = std::process::Command::new("cmake");
        build_cmd.current_dir(&build_dir)
            .arg("--build").arg(".")
            .arg("--config").arg("Release")
            .arg("--parallel").arg(format!("{}", threads.get()));
            
        println!("Running manual cmake build: {:?}", build_cmd);
        let status = build_cmd.status()?;
        if !status.success() {
            anyhow::bail!("Manual CMake build failed");
        }

        let mut install_cmd = std::process::Command::new("cmake");
        install_cmd.current_dir(&build_dir)
            .arg("--install").arg(".");
            
        println!("Running manual cmake install: {:?}", install_cmd);
        let status = install_cmd.status()?;
        if !status.success() {
            anyhow::bail!("Manual CMake install failed");
        }

        return Ok(());
    }

    let mut config = cmake::Config::new(path);
    
    config.define("CMAKE_CXX_STANDARD", "14")
        .parallel(threads.get() as u8)
        .define("MNN_BUILD_SHARED_LIBS", "OFF")
        .define("MNN_SEP_BUILD", "OFF")
        .define("MNN_PORTABLE_BUILD", "ON")
        .define("MNN_USE_SYSTEM_LIB", "OFF")
        .define("MNN_BUILD_CONVERTER", "OFF")
        .define("MNN_BUILD_TOOLS", "OFF")
        .define("CMAKE_INSTALL_PREFIX", install.as_ref());



    // https://github.com/rust-lang/rust/issues/39016
    // https://github.com/rust-lang/cc-rs/pull/717
    // .define("CMAKE_BUILD_TYPE", "Release")
    
    config.pipe(|mut config| {
            config.define("MNN_WIN_RUNTIME_MT", CxxOption::CRT_STATIC.cmake_value());
            config.define("MNN_USE_THREAD_POOL", CxxOption::THREADPOOL.cmake_value());
            config.define("MNN_OPENMP", CxxOption::OPENMP.cmake_value());
            config.define("MNN_VULKAN", CxxOption::VULKAN.cmake_value());
            config.define("MNN_METAL", CxxOption::METAL.cmake_value());
            config.define("MNN_COREML", CxxOption::COREML.cmake_value());
            config.define("MNN_OPENCL", CxxOption::OPENCL.cmake_value());
            config.define("MNN_OPENGL", CxxOption::OPENGL.cmake_value());
            config.define("MNN_USE_SSE", "ON");
            // config.define("CMAKE_CXX_FLAGS", "-O0");
            // #[cfg(windows)]
            if *TARGET_OS == "windows" {
                config.define("CMAKE_CXX_FLAGS", "-DWIN32=1 -msse4.1");
                config.define("CMAKE_C_FLAGS", "-DWIN32=1 -msse4.1");
            }

            if is_emscripten() {
                config
                    .define("CMAKE_C_COMPILER", "emcc")
                    .define("CMAKE_CXX_COMPILER", "em++")
                    .target("wasm32-unknown-emscripten");
            }
            config
        })
        .build();
    Ok(())
}

// pub fn try_patch_file(patch: impl AsRef<Path>, file: impl AsRef<Path>) -> Result<()> {
//     let patch = dunce::canonicalize(patch)?;
//     rerun_if_changed(&patch);
//     let patch = std::fs::read_to_string(&patch)?;
//     let patch = diffy::Patch::from_str(&patch)?;
//     let file_path = file.as_ref();
//     let file = std::fs::read_to_string(file_path).context("Failed to read input file")?;
//     let patched_file =
//         diffy::apply(&file, &patch).context("Failed to apply patches using diffy")?;
//     std::fs::write(file_path, patched_file)?;
//     Ok(())
// }

pub fn rerun_if_changed(path: impl AsRef<Path>) {
    println!("cargo:rerun-if-changed={}", path.as_ref().display());
}

// pub fn vulkan_includes(vendor: impl AsRef<Path>) -> Vec<PathBuf> {
//     let vendor = vendor.as_ref();
//     let vulkan_dir = vendor.join("source/backend/vulkan");
//     if cfg!(feature = "vulkan") {
//         vec![
//             vulkan_dir.clone(),
//             vulkan_dir.join("runtime"),
//             vulkan_dir.join("component"),
//             // IDK If the order is important but the cmake file does it like this
//             vulkan_dir.join("buffer/execution"),
//             vulkan_dir.join("buffer/backend"),
//             vulkan_dir.join("buffer"),
//             vulkan_dir.join("buffer/shaders"),
//             // vulkan_dir.join("image/execution"),
//             // vulkan_dir.join("image/backend"),
//             // vulkan_dir.join("image"),
//             // vulkan_dir.join("image/shaders"),
//             vendor.join("schema/current"),
//             vendor.join("3rd_party/flatbuffers/include"),
//             vendor.join("source"),
//         ]
//     } else {
//         vec![]
//     }
// }

pub fn is_emscripten() -> bool {
    *TARGET_OS == "emscripten" && *TARGET_ARCH == "wasm32"
}

pub fn emscripten_cache() -> &'static str {
    &EMSCRIPTEN_CACHE
}

#[derive(Debug, Clone, Copy)]
pub enum CxxOptionValue {
    On,
    Off,
    Value(&'static str),
}

impl From<bool> for CxxOptionValue {
    fn from(b: bool) -> Self {
        if b {
            Self::On
        } else {
            Self::Off
        }
    }
}

impl CxxOptionValue {
    pub const fn from_bool(value: bool) -> Self {
        match value {
            true => Self::On,
            false => Self::Off,
        }
    }
}

impl From<&'static str> for CxxOptionValue {
    fn from(s: &'static str) -> Self {
        match s {
            "ON" => Self::On,
            "OFF" => Self::Off,
            _ => Self::Value(s),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CxxOption {
    pub name: &'static str,
    pub value: CxxOptionValue,
}

macro_rules! cxx_option_from_feature {
    ($feature:literal, $cxx:literal) => {{
        CxxOption::from_bool($cxx, cfg!(feature = $feature))
    }};
}
impl CxxOption {
    const fn from_bool(name: &'static str, value: bool) -> Self {
        Self {
            name,
            value: CxxOptionValue::from_bool(value),
        }
    }
    pub const VULKAN: CxxOption = cxx_option_from_feature!("vulkan", "MNN_VULKAN");
    pub const METAL: CxxOption = cxx_option_from_feature!("metal", "MNN_METAL");
    pub const COREML: CxxOption = cxx_option_from_feature!("coreml", "MNN_COREML");
    pub const OPENCL: CxxOption = cxx_option_from_feature!("opencl", "MNN_OPENCL");
    pub const OPENMP: CxxOption = cxx_option_from_feature!("openmp", "MNN_OPENMP");
    pub const OPENGL: CxxOption = cxx_option_from_feature!("opengl", "MNN_OPENGL");
    pub const CRT_STATIC: CxxOption = cxx_option_from_feature!("crt_static", "MNN_WIN_RUNTIME_MT");
    pub const THREADPOOL: CxxOption =
        cxx_option_from_feature!("mnn-threadpool", "MNN_USE_THREAD_POOL");

    pub fn new(name: &'static str, value: impl Into<CxxOptionValue>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }

    pub fn on(mut self) -> Self {
        self.value = CxxOptionValue::On;
        self
    }

    pub fn off(mut self) -> Self {
        self.value = CxxOptionValue::Off;
        self
    }

    pub fn with_value(mut self, value: &'static str) -> Self {
        self.value = CxxOptionValue::Value(value);
        self
    }

    pub fn cmake(&self) -> String {
        match &self.value {
            CxxOptionValue::On => format!("-D{}=ON", self.name),
            CxxOptionValue::Off => format!("-D{}=OFF", self.name),
            CxxOptionValue::Value(v) => format!("-D{}={}", self.name, v),
        }
    }

    pub fn cmake_value(&self) -> &'static str {
        match &self.value {
            CxxOptionValue::On => "ON",
            CxxOptionValue::Off => "OFF",
            CxxOptionValue::Value(v) => v,
        }
    }

    pub fn cxx(&self) -> String {
        match &self.value {
            CxxOptionValue::On => format!("-D{}=1", self.name),
            CxxOptionValue::Off => format!("-D{}=0", self.name),
            CxxOptionValue::Value(v) => format!("-D{}={}", self.name, v),
        }
    }

    pub fn enabled(&self) -> bool {
        match self.value {
            CxxOptionValue::On => true,
            CxxOptionValue::Off => false,
            CxxOptionValue::Value(_) => true,
        }
    }
}
