//! Small time/format helpers.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn today() -> String {
    let now = chrono::Local::now();
    format!("{}", now.format("%Y-%m-%d"))
}

pub fn today_with_time() -> String {
    let now = chrono::Local::now();
    format!("{}", now.format("%Y-%m-%dT%H:%M:%S%z"))
}
