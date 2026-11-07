// When this build was made (Unix seconds), for the status line -- so it's
// clear which build a browser or phone is really running.
fn main() {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("cargo:rustc-env=HPSDR_REMOTE_BUILT={secs}");
    println!("cargo:rerun-if-changed=src");
}
