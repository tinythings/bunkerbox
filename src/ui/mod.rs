pub mod tui;

pub fn is_launch_mode() -> bool {
    crate::cfg::RuntimeConfig::invoked_name().map(|n| n != "bunkerbox").unwrap_or(false)
}
