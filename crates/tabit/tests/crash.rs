//! The crash contract (FRONTEND.md §3.5, owner ruling): an internal
//! error — a panic anywhere in the process — crashes the process with
//! exit code 101 and a stderr report, never lingers as a zombie
//! holding a live stdin. End-to-end through the real binary, because
//! the property spans the panic hook, the runtime, and process exit.

#![cfg_attr(
    test,
    allow(
        clippy::err_expect,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::panic_in_result_fn,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::process::Command;

#[test]
fn an_internal_error_crashes_with_code_101_and_a_stderr_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_tabit"))
        .env("TABIT_CRASH_TEST", "1")
        .output()
        .expect("spawn the tabit binary");
    assert_eq!(output.status.code(), Some(101), "panics exit 101");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("injected internal error"),
        "the report carries the panic message: {stderr}"
    );
    assert!(
        stderr.contains("report this"),
        "the report tells the user what to do: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "no protocol bytes after a crash injection"
    );
}
