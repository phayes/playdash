//! The wall clock a stream asks which media segments are available now.

use std::fmt::Debug;
use std::time::SystemTime;

/// A source of the current wall-clock time.
pub(crate) trait Clock: Debug + Send + Sync {
    /// The time now.
    fn now(&self) -> SystemTime;
}

/// The system's wall clock.
#[derive(Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
