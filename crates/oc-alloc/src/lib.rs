//! Dedicated allocator FFI crate.
//!
//! `unsafe_code` is forbidden across the rest of the workspace (`AGENTS.md`);
//! this isolated crate encapsulates the tiny `libmimalloc-sys` option/purge calls
//! with explicit safety proofs so the server configures `mimalloc` for low
//! idle RSS (`<= 60 MB`) and immediate OS page return (`madvise(MADV_DONTNEED)`).

pub use mimalloc::MiMalloc;

const MI_OPTION_ARENA_EAGER_COMMIT: libmimalloc_sys::mi_option_t = 4;
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
const MI_OPTION_ARENA_RESERVE: libmimalloc_sys::mi_option_t = 23;
const MI_OPTION_PAGE_FULL_RETAIN: libmimalloc_sys::mi_option_t = 36;
const MI_OPTION_ALLOW_THP: libmimalloc_sys::mi_option_t = 43;

/// Configure `mimalloc` for server workloads with tight idle memory budgets.
///
/// Disables eager whole-arena commits, removes the 1000 ms v3 purge delay so
/// freed startup buffers return to the kernel immediately, caps initial arena
/// reservation at 64 MiB instead of 1 GiB, stops retaining idle full small pages,
/// and disables transparent huge pages.
#[allow(unsafe_code)]
pub fn configure_low_footprint() {
    // SAFETY: `mi_option_set` updates process-wide entries in mimalloc's
    // static option table; option indices match `v3/include/mimalloc.h` (`enum
    // mi_option_e`) and values are non-negative scalars in documented ranges.
    unsafe {
        libmimalloc_sys::mi_option_set(MI_OPTION_ARENA_EAGER_COMMIT, 0);
        libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0);
        libmimalloc_sys::mi_option_set(MI_OPTION_ARENA_RESERVE, 65_536);
        libmimalloc_sys::mi_option_set(MI_OPTION_PAGE_FULL_RETAIN, 0);
        libmimalloc_sys::mi_option_set(MI_OPTION_ALLOW_THP, 0);
        libmimalloc_sys::mi_collect(true);
    }
}

/// Aggressively return freed pages across heaps to the OS (`mi_collect(true)`).
#[allow(unsafe_code)]
pub fn purge_idle_memory() {
    // SAFETY: `mi_collect(true)` takes a plain boolean and performs mimalloc's
    // documented internal cross-heap sweep/purge synchronization.
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(unsafe_code)]
    fn low_footprint_options_apply_cleanly() {
        configure_low_footprint();
        // SAFETY: `mi_option_get` reads validated static option indices.
        unsafe {
            assert_eq!(libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY), 0);
            assert_eq!(
                libmimalloc_sys::mi_option_get(MI_OPTION_ARENA_EAGER_COMMIT),
                0
            );
            assert_eq!(
                libmimalloc_sys::mi_option_get(MI_OPTION_PAGE_FULL_RETAIN),
                0
            );
            assert_eq!(libmimalloc_sys::mi_option_get(MI_OPTION_ALLOW_THP), 0);
        }
        purge_idle_memory();
    }
}
