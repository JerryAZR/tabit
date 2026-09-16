//! Identity and time stamps for session records.
//!
//! Session and entry ids are UUIDv7: time-ordered (so unsorted directory
//! listings still sort by creation time), collision-free without
//! coordination, and single-line in logs.

use std::time::SystemTime;

/// A fresh session id.
pub fn new_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// A fresh entry id.
pub fn new_entry_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The current time as an RFC 3339 string (second precision), for human
/// reading and diffing of session files.
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339(SystemTime::now()).to_string()
}

/// A filesystem-safe variant of an RFC 3339 timestamp for session file
/// names (`:` is illegal in Windows file names).
pub fn filename_timestamp() -> String {
    now_rfc3339().replace(':', "-")
}

/// The current time as milliseconds since the Unix epoch — the wire
/// form the protocol's bracket timestamps carry (codex's
/// `started_at_ms`/`completed_at_ms` shape).
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// An entry's RFC 3339 timestamp back to Unix milliseconds — replay's
/// source for the same bracket timestamps the live run stamps at
/// emission. `None` when the string is not parseable (a corrupt
/// timestamp; the caller decides how loud to be).
pub fn rfc3339_to_unix_ms(timestamp: &str) -> Option<u64> {
    humantime::parse_rfc3339(timestamp)
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as u64)
}
