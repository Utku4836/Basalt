//! Lifecycle-based status and monotonic working duration, never output guesses.
use std::time::{Duration, Instant};
pub fn state(signal: Option<&crate::agent_bridge::Signal>, exited: bool, failed: bool) -> i32 {
    if failed {
        return 5;
    }
    if exited {
        return 6;
    }
    let Some(signal) = signal else {
        return 2;
    };
    match signal.state.as_str() {
        "working" => 1,
        "waiting" => 3,
        "completed" => 4,
        "error" => 5,
        "closed" => 6,
        _ => match signal.event.as_str() {
            "PermissionRequest" => 3,
            "Stop" => 4,
            "StopFailure" => 5,
            _ => 0,
        },
    }
}
pub struct Clock {
    state: i32,
    start: Instant,
    elapsed: Duration,
}
impl Default for Clock {
    fn default() -> Self {
        Self {
            state: 2,
            start: Instant::now(),
            elapsed: Duration::ZERO,
        }
    }
}
impl Clock {
    pub fn update(&mut self, state: i32, now: Instant) {
        if state == self.state {
            return;
        }
        if self.state == 1 {
            self.elapsed += now.saturating_duration_since(self.start);
        }
        if state == 1 {
            self.elapsed = Duration::ZERO;
            self.start = now;
        }
        self.state = state;
    }
    pub fn label(&self, now: Instant) -> String {
        let elapsed = self.elapsed
            + if self.state == 1 {
                now.saturating_duration_since(self.start)
            } else {
                Duration::ZERO
            };
        if elapsed.is_zero() {
            String::new()
        } else {
            format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
        }
    }
}
