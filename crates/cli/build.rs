// rustc hands a crate its target triple only as separate `cfg` values, and
// reassembling "x86_64-unknown-linux-gnu" from those is guesswork the moment
// a libc or ABI variant is involved. Cargo does pass the real triple to build
// scripts, so record it here: `self-update` has to name the exact release
// asset it was built for.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("cargo always sets TARGET for build scripts");
    println!("cargo::rustc-env=MCPGW_TARGET={target}");
}
