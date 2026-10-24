#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::run;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::run;
