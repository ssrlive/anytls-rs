#[cfg(feature = "ffi")]
use std::{
    ffi::c_void,
    sync::{LazyLock, Mutex},
    time::Instant,
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrafficStatus {
    /// Bytes sent from local proxy clients to the AnyTLS server.
    pub tx: u64,
    /// Bytes received from the AnyTLS server for local proxy clients.
    pub rx: u64,
}

#[cfg(feature = "ffi")]
#[derive(Clone, Copy)]
struct TrafficStatusCallback {
    callback: unsafe extern "C" fn(*const TrafficStatus, *mut c_void),
    ctx: *mut c_void,
}

#[cfg(feature = "ffi")]
unsafe impl Send for TrafficStatusCallback {}

#[cfg(feature = "ffi")]
impl TrafficStatusCallback {
    unsafe fn call(self, status: &TrafficStatus) {
        unsafe { (self.callback)(status, self.ctx) };
    }
}

#[cfg(feature = "ffi")]
#[derive(Debug)]
struct TrafficTracker {
    status: TrafficStatus,
    last_reported: Instant,
}

#[cfg(feature = "ffi")]
impl TrafficTracker {
    fn update(&mut self, tx: usize, rx: usize, interval_secs: u32, now: Instant) -> Option<TrafficStatus> {
        self.status.tx = self.status.tx.saturating_add(tx as u64);
        self.status.rx = self.status.rx.saturating_add(rx as u64);
        if now.duration_since(self.last_reported).as_secs() >= u64::from(interval_secs) {
            self.last_reported = now;
            Some(self.status)
        } else {
            None
        }
    }
}

#[cfg(feature = "ffi")]
static CALLBACK: Mutex<Option<TrafficStatusCallback>> = Mutex::new(None);
#[cfg(feature = "ffi")]
static SEND_INTERVAL_SECS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
#[cfg(feature = "ffi")]
static TRACKER: LazyLock<Mutex<TrafficTracker>> = LazyLock::new(|| {
    Mutex::new(TrafficTracker {
        status: TrafficStatus::default(),
        last_reported: Instant::now(),
    })
});

#[cfg(feature = "ffi")]
pub(crate) fn set_callback(
    send_interval_secs: u32,
    callback: Option<unsafe extern "C" fn(*const TrafficStatus, *mut c_void)>,
    ctx: *mut c_void,
) {
    if send_interval_secs > 0 {
        SEND_INTERVAL_SECS.store(send_interval_secs, std::sync::atomic::Ordering::Relaxed);
    }

    let mut registered = CALLBACK.lock().unwrap_or_else(|error| error.into_inner());
    *registered = callback.map(|callback| TrafficStatusCallback { callback, ctx });
    if registered.is_some() {
        let mut tracker = TRACKER.lock().unwrap_or_else(|error| error.into_inner());
        tracker.last_reported = Instant::now();
    }
}

#[cfg(feature = "ffi")]
pub(crate) fn record(tx: usize, rx: usize) {
    if tx == 0 && rx == 0 {
        return;
    }
    let callback = *CALLBACK.lock().unwrap_or_else(|error| error.into_inner());
    let Some(callback) = callback else {
        return;
    };

    let status = {
        let mut tracker = TRACKER.lock().unwrap_or_else(|error| error.into_inner());
        tracker.update(
            tx,
            rx,
            SEND_INTERVAL_SECS.load(std::sync::atomic::Ordering::Relaxed),
            Instant::now(),
        )
    };
    if let Some(status) = status {
        unsafe { callback.call(&status) };
    }
}

#[cfg(all(test, feature = "ffi"))]
mod tests {
    use super::{TrafficStatus, TrafficTracker};
    use std::time::{Duration, Instant};

    #[test]
    fn accumulates_directions_and_reports_only_after_interval() {
        let start = Instant::now();
        let mut tracker = TrafficTracker {
            status: TrafficStatus::default(),
            last_reported: start,
        };

        assert_eq!(tracker.update(10, 3, 2, start + Duration::from_secs(1)), None);
        assert_eq!(
            tracker.update(4, 8, 2, start + Duration::from_secs(2)),
            Some(TrafficStatus { tx: 14, rx: 11 })
        );
    }
}
