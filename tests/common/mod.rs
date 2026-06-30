// Shared helpers for smoke (LUNAR_SMOKE=1) integration tests.
// This file is NOT compiled as a test binary; it is included via `mod common;`
// from each smoke test file.

/// Returns true when LUNAR_SMOKE is set to "1", enabling real-infra paths.
pub fn smoke_enabled() -> bool {
    std::env::var("LUNAR_SMOKE").as_deref() == Ok("1")
}
