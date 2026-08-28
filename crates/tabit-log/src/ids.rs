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
