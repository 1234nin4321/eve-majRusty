//! Monotonic tick counter.

use windows_sys::Win32::System::SystemInformation::GetTickCount64;

/// Monotonic milliseconds since boot (GetTickCount64) — for elapsed-time/duration comparisons only, never wall-clock/calendar time.
/// A distinct type (not a bare u64) so a timestamp from a different clock source can't be silently compared against one of these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticks {
    pub ms: u64,
}

impl Ticks {
    pub fn now() -> Ticks {
        Ticks { ms: unsafe { GetTickCount64() } }
    }

    /// Elapsed ms from `earlier` to `self`; saturates at 0 rather than underflowing if `earlier` is somehow later.
    pub fn elapsed_since(self, earlier: Ticks) -> u64 {
        self.ms.saturating_sub(earlier.ms)
    }

    pub fn is_zero(self) -> bool {
        self.ms == 0
    }
}
