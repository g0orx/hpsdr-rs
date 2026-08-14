/*
    Builds WDSP, libspecbleach, and rnnoise from vendored C source
    (vendor/wdsp, vendor/libspecbleach, vendor/rnnoise) instead of linking
    prebuilt platform-specific binaries -- ported from rustyHPSDR's own
    build.rs (same source, same file list), which already builds this
    exact source tree on both Linux and Windows (MSYS2/MinGW-w64).

    Confirmed viable cross-platform by reading the vendored source
    directly: wdsp/comm.h and wdsp/linux_port.h branch on
    `#if defined(linux) || defined(__APPLE__)` vs `#ifdef _WIN32` --  the
    Windows branch just pulls in the real <Windows.h>/<avrt.h>/<intrin.h>
    and uses real Win32 types (CRITICAL_SECTION, HANDLE, etc.) directly.
    That's WDSP's original Windows-native code path (Thetis's own
    platform); linux_port.c's pthread-based shims exist to fake that same
    Win32-shaped API on Linux/macOS, not the other way around -- so
    building under MinGW-w64 (which defines _WIN32, not linux) takes the
    same real-Windows-API path MSVC would, no extra porting needed here.

    NOTE on a bug this replaced: the three vendor .a files this used to
    link were confirmed (via `md5sum`/`nm`) to be three byte-identical
    copies of one combined archive, not three separate libraries --
    traced to rustyHPSDR's own build.rs calling `.compile()` three times
    on one cc::Build that already had every file from all three projects
    added to it, so each call redundantly recompiled everything under a
    different name. Fixed here by calling `.compile()` once.
*/

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    let fftw = pkg_config::probe_library("fftw3")
        .expect("Could not find fftw3. Linux: `apt install libfftw3-dev` (or your distro's equivalent). Windows (MSYS2/MinGW-w64): `pacman -S mingw-w64-x86_64-fftw`.");

    let mut build = cc::Build::new();
    build.files([
        "vendor/libspecbleach/src/processors/specbleach_adenoiser.c",
        "vendor/libspecbleach/src/processors/specbleach_denoiser.c",
        "vendor/libspecbleach/src/processors/adaptivedenoiser/adaptive_denoiser.c",
        "vendor/libspecbleach/src/processors/denoiser/spectral_denoiser.c",
        "vendor/libspecbleach/src/shared/stft/stft_windows.c",
        "vendor/libspecbleach/src/shared/stft/fft_transform.c",
        "vendor/libspecbleach/src/shared/stft/stft_buffer.c",
        "vendor/libspecbleach/src/shared/stft/stft_processor.c",
        "vendor/libspecbleach/src/shared/noise_estimation/noise_estimator.c",
        "vendor/libspecbleach/src/shared/noise_estimation/noise_profile.c",
        "vendor/libspecbleach/src/shared/noise_estimation/adaptive_noise_estimator.c",
        "vendor/libspecbleach/src/shared/utils/general_utils.c",
        "vendor/libspecbleach/src/shared/utils/spectral_features.c",
        "vendor/libspecbleach/src/shared/utils/spectral_trailing_buffer.c",
        "vendor/libspecbleach/src/shared/utils/denoise_mixer.c",
        "vendor/libspecbleach/src/shared/utils/spectral_utils.c",
        "vendor/libspecbleach/src/shared/gain_estimation/gain_estimators.c",
        "vendor/libspecbleach/src/shared/post_estimation/spectral_whitening.c",
        "vendor/libspecbleach/src/shared/post_estimation/noise_floor_manager.c",
        "vendor/libspecbleach/src/shared/post_estimation/postfilter.c",
        "vendor/libspecbleach/src/shared/pre_estimation/absolute_hearing_thresholds.c",
        "vendor/libspecbleach/src/shared/pre_estimation/spectral_smoother.c",
        "vendor/libspecbleach/src/shared/pre_estimation/noise_scaling_criterias.c",
        "vendor/libspecbleach/src/shared/pre_estimation/critical_bands.c",
        "vendor/libspecbleach/src/shared/pre_estimation/masking_estimator.c",
        "vendor/libspecbleach/src/shared/pre_estimation/transient_detector.c",
        "vendor/rnnoise/src/denoise.c",
        "vendor/rnnoise/src/celt_lpc.c",
        "vendor/rnnoise/src/kiss_fft.c",
        "vendor/rnnoise/src/nnet.c",
        "vendor/rnnoise/src/nnet_default.c",
        "vendor/rnnoise/src/parse_lpcnet_weights.c",
        "vendor/rnnoise/src/pitch.c",
        "vendor/rnnoise/src/rnn.c",
        "vendor/rnnoise/src/rnnoise_data.c",
        "vendor/rnnoise/src/rnnoise_tables.c",
        "vendor/wdsp/FDnoiseIQ.c",
        "vendor/wdsp/calculus.c",
        "vendor/wdsp/emnr.c",
        "vendor/wdsp/icfir.c",
        "vendor/wdsp/meter.c",
        "vendor/wdsp/shift.c",
        "vendor/wdsp/RXA.c",
        "vendor/wdsp/cblock.c",
        "vendor/wdsp/emph.c",
        "vendor/wdsp/iir.c",
        "vendor/wdsp/meterlog10.c",
        "vendor/wdsp/siphon.c",
        "vendor/wdsp/TXA.c",
        "vendor/wdsp/cfcomp.c",
        "vendor/wdsp/eq.c",
        "vendor/wdsp/impulse_cache.c",
        "vendor/wdsp/nbp.c",
        "vendor/wdsp/slew.c",
        "vendor/wdsp/amd.c",
        "vendor/wdsp/cfir.c",
        "vendor/wdsp/fcurve.c",
        "vendor/wdsp/iobuffs.c",
        "vendor/wdsp/nob.c",
        "vendor/wdsp/snb.c",
        "vendor/wdsp/ammod.c",
        "vendor/wdsp/channel.c",
        "vendor/wdsp/fir.c",
        "vendor/wdsp/iqc.c",
        "vendor/wdsp/nobII.c",
        "vendor/wdsp/ssql.c",
        "vendor/wdsp/amsq.c",
        "vendor/wdsp/cmath.c",
        "vendor/wdsp/firmin.c",
        "vendor/wdsp/linux_port.c",
        "vendor/wdsp/osctrl.c",
        "vendor/wdsp/syncbuffs.c",
        "vendor/wdsp/analyzer.c",
        "vendor/wdsp/compress.c",
        "vendor/wdsp/fmd.c",
        "vendor/wdsp/lmath.c",
        "vendor/wdsp/patchpanel.c",
        "vendor/wdsp/utilities.c",
        "vendor/wdsp/anf.c",
        "vendor/wdsp/delay.c",
        "vendor/wdsp/fmmod.c",
        "vendor/wdsp/main.c",
        "vendor/wdsp/resample.c",
        "vendor/wdsp/varsamp.c",
        "vendor/wdsp/anr.c",
        "vendor/wdsp/dexp.c",
        "vendor/wdsp/fmsq.c",
        "vendor/wdsp/rmatch.c",
        "vendor/wdsp/version.c",
        "vendor/wdsp/apfshadow.c",
        "vendor/wdsp/div.c",
        "vendor/wdsp/gain.c",
        "vendor/wdsp/rnnr.c",
        "vendor/wdsp/wcpAGC.c",
        "vendor/wdsp/bandpass.c",
        "vendor/wdsp/doublepole.c",
        "vendor/wdsp/gaussian.c",
        "vendor/wdsp/sbnr.c",
        "vendor/wdsp/wisdom.c",
        "vendor/wdsp/calcc.c",
        "vendor/wdsp/eer.c",
        "vendor/wdsp/gen.c",
        "vendor/wdsp/matchedCW.c",
        "vendor/wdsp/sender.c",
        "vendor/wdsp/zetaHat.c",
    ]);

    build.include("vendor/libspecbleach/include");
    build.include("vendor/libspecbleach/src");
    build.include("vendor/libspecbleach/src/processors");
    build.include("vendor/libspecbleach/src/processors/adaptivedenoiser");
    build.include("vendor/libspecbleach/src/processors/denoiser");
    build.include("vendor/libspecbleach/src/shared/stft");
    build.include("vendor/libspecbleach/src/shared/noise_estimation");
    build.include("vendor/libspecbleach/src/shared");
    build.include("vendor/libspecbleach/src/shared/utils");
    build.include("vendor/libspecbleach/src/shared/gain_estimation");
    build.include("vendor/libspecbleach/src/shared/post_estimation");
    build.include("vendor/libspecbleach/src/shared/pre_estimation");
    build.include("vendor/wdsp");
    build.include("vendor/rnnoise/src");
    build.include("vendor/rnnoise/include");

    // Same flags rustyHPSDR's own build.rs uses -- MinGW-w64's GCC
    // accepts all of these identically to Linux GCC/Clang, so no
    // per-OS branching is needed here (confirmed: -pthread and
    // -D_GNU_SOURCE are harmless no-ops under MinGW's libc, not
    // Linux-glibc-only landmines).
    build.flag("-O3");
    build.flag("-pthread");
    build.flag("-D_GNU_SOURCE");
    build.flag("-Wno-parentheses");
    build.flag("-march=native");

    for path in fftw.include_paths {
        build.include(path);
    }

    // One combined static lib -- see this file's top doc comment for why
    // this is deliberately ONE compile() call, not three.
    build.compile("wdsp");

    // fftw3f (float precision) is needed alongside fftw3 (double) for
    // this project's spectrum analyzer, on top of whatever RXA/TXA
    // itself uses in double precision -- confirmed via the previous
    // prebuilt-lib build.rs's own undefined-symbol inspection. Not
    // present in rustyHPSDR's own build.rs (it apparently doesn't need
    // the float path), kept here since this project does.
    println!("cargo:rustc-link-lib=fftw3");
    println!("cargo:rustc-link-lib=fftw3f");

    // pthread/libm are separate system libs to link against on
    // Linux/macOS; on Windows these are either not separate libs at all
    // (libm's contents are part of the C runtime) or not needed the same
    // way (MinGW's pthread support doesn't require this), and comm.h's
    // <avrt.h> (Windows Multimedia Class Scheduler Service, used for
    // real-time thread priority) needs avrt linked instead -- MinGW-w64
    // ships this.
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=avrt");
    } else {
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=m");
    }

    println!("cargo:rerun-if-changed=vendor/wdsp");
    println!("cargo:rerun-if-changed=vendor/libspecbleach");
    println!("cargo:rerun-if-changed=vendor/rnnoise");
}
