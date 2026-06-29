#[cfg(all(target_os = "linux", feature = "fuser"))]
pub mod linux;

#[cfg(all(target_os = "macos", feature = "fuse"))]
pub mod macos;

// projfs_logic is target-agnostic: compiled and testable on all platforms.
pub mod projfs_logic;

#[cfg(all(target_os = "windows", feature = "projfs"))]
pub mod windows;
