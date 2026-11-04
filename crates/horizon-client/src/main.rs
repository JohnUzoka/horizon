//! Horizon: native Rust client for Minecraft Java Edition 26.3 gameplay.
#![forbid(unsafe_code)]

// The Switch build uses mimalloc too, unless built with `nx-allocator`: then horizon-nx's
// allocator (its `allocator.rs`) is the global one and declares itself.
#[cfg(not(any(
    feature = "system-allocator",
    feature = "alloc-census",
    all(target_os = "horizon", feature = "nx-allocator")
)))]
#[global_allocator]
static ALLOCATOR: horizon_threads::Allocator = horizon_threads::ALLOCATOR;
#[cfg(all(feature = "alloc-census", not(feature = "system-allocator")))]
#[global_allocator]
static ALLOCATOR: horizon_threads::CensusAllocator = horizon_threads::CensusAllocator;

/// The mod API level this game serves, kept in the program for the launcher
/// to read without running it (`horizon_mods::api_level::MARK`). `main`
/// reads it, so the linker keeps it.
static MOD_API_LEVEL: [u8; 27] = horizon_mods::api_level::MARK;

fn main() {
    // Switch: mount romfs, bring up BSD sockets, set the applet focus mode and open `log.txt`
    // before anything allocates. Also what pulls the libnx lifecycle hooks into the link.
    #[cfg(target_os = "horizon")]
    horizon_nx::init();
    std::hint::black_box(&MOD_API_LEVEL);
    horizon_client::entry::main();
}
