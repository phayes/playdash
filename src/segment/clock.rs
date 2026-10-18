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

/// Controllable test clock that advances only when explicitly set.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock(std::sync::Mutex<SystemTime>);

#[cfg(test)]
impl ManualClock {
    /// Creates a clock at `secs` seconds after the Unix epoch.
    pub(crate) fn at(secs: u64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(std::sync::Mutex::new(Self::time(secs))))
    }

    /// Moves the clock to `secs` seconds after the Unix epoch.
    pub(crate) fn set(&self, secs: u64) {
        *self.0.lock().unwrap() = Self::time(secs);
    }

    fn time(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}
