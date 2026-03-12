pub use tracing::{debug, error, info, warn};

/// Initialize a no-op tracing subscriber (for tests).
///
/// This sets a subscriber that discards all events, which is useful
/// for suppressing log output during test runs. Safe to call multiple times;
/// subsequent calls after the first are silently ignored.
pub fn init_nop_logger() {
    use tracing::subscriber::NoSubscriber;
    let _ = tracing::subscriber::set_global_default(NoSubscriber::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_nop_logger() {
        init_nop_logger();
        init_nop_logger(); // calling twice must not panic
    }
}
