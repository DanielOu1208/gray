pub mod contract;
pub mod exit;
pub mod fence;
pub mod kill;
pub mod label;
pub mod pump;
pub mod spawn;
pub mod split;
pub mod tools;
pub mod view;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::shell_path;
