#[cfg(all(feature = "compress", feature = "util", not(target_arch = "wasm32")))]
pub(crate) mod compress;
#[cfg(all(feature = "util", not(target_arch = "wasm32")))]
pub(crate) mod decompress;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) mod wasm;
// The WASM entry points' per-member decisions, also built for host tests.
#[cfg(any(test, all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) mod wasm_rules;
