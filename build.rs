use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let vendor_dir = manifest_dir.join("vendor");

    println!("cargo:rustc-link-search=native={}", vendor_dir.display());
    println!("cargo:rustc-link-lib=static=wdsp");

    // NR3/NR4 (RXA's RNNR and SBNR noise reduction stages) are backed
    // by two separate vendored static libs, not part of libwdsp.a
    // itself. Both must come after libwdsp on the link line (GNU ld
    // resolves left-to-right, so a library needs to appear after
    // whatever references its symbols) -- same reasoning as fftw3
    // below, just one step earlier in the chain.
    //
    // liblibspecbleach.a's doubled "lib" in the filename isn't a typo
    // -- libspecbleach's own CMake build names its target
    // "libspecbleach", so its default archive output naming prepends
    // another "lib" on top of that. The link name below strips only
    // the leading "lib" and the ".a" extension, same as any other
    // static lib, which is why it ends up as "libspecbleach" rather
    // than "specbleach".
    //
    // Neither of these has been confirmed against your reference for
    // its own further dependencies (e.g. whether libspecbleach uses
    // FFTW3 internally or its own FFT) -- if the linker comes back
    // with undefined symbols from either, that's the first thing to
    // check, and the fix is likely reordering relative to the fftw3
    // lines below rather than anything in this project's own code.
    println!("cargo:rustc-link-lib=static=libspecbleach");
    println!("cargo:rustc-link-lib=static=rnnoise");

    // libwdsp.a's external dependencies, confirmed by inspecting its
    // undefined symbols: FFTW3 (both double and float precision, used
    // for the RXA/TXA chains and the spectrum analyzer respectively),
    // pthreads, and libm.
    //
    // These assume FFTW3's dev packages are installed system-wide
    // (e.g. `apt install libfftw3-dev` on Debian/Ubuntu, or the
    // equivalent for your distro/OS). Adjust if your linker can't find
    // them -- e.g. add `cargo:rustc-link-search=native=<path>` for a
    // non-standard FFTW install location.
    println!("cargo:rustc-link-lib=fftw3");
    println!("cargo:rustc-link-lib=fftw3f");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=m");

    println!("cargo:rerun-if-changed=vendor/libwdsp.a");
    println!("cargo:rerun-if-changed=vendor/liblibspecbleach.a");
    println!("cargo:rerun-if-changed=vendor/librnnoise.a");
}
