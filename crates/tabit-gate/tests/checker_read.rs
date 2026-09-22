//! The read checker over the shipped default config (ported from
//! pi-sanity `tests/integration/checker/read.test.ts`): regular files,
//! secret files in HOME, non-secret hidden files, the node_modules
//! exception, hidden files in CWD, and system directories.

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

mod common;

use common::{assert_action, home, tmpdir};
use tabit_gate::checker_read::check_read;
use tabit_gate::config;
use tabit_gate::types::Action;

// --- regular files -------------------------------------------------------

#[test]
fn allows_reading_regular_files_in_home() {
    let path = format!("{}/documents/file.txt", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "regular HOME files read freely");
}

#[test]
fn allows_reading_regular_files_in_cwd() {
    let result = check_read("package.json", &config::default_config());
    assert_action(&result, Action::Allow, "regular CWD files read freely");
}

#[test]
fn allows_reading_regular_files_in_tmpdir() {
    let path = format!("{}/temp.txt", tmpdir());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "TMPDIR files read freely");
}

// --- secret files in HOME ------------------------------------------------

#[test]
fn asks_for_private_ssh_key() {
    let path = format!("{}/.ssh/id_rsa", home());
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "~/.ssh/id_rsa may contain credentials",
    );
}

#[test]
fn asks_for_aws_credentials() {
    let path = format!("{}/.aws/credentials", home());
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "~/.aws/credentials may contain credentials",
    );
}

#[test]
fn asks_for_netrc() {
    let path = format!("{}/.netrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Ask, "~/.netrc may contain credentials");
}

#[test]
fn allows_ssh_public_key() {
    let path = format!("{}/.ssh/id_rsa.pub", home());
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "public keys are an explicit exception",
    );
}

// --- non-secret hidden files in HOME --------------------------------------

#[test]
fn allows_bashrc() {
    let path = format!("{}/.bashrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "shell config is not a secret");
}

#[test]
fn allows_zshrc() {
    let path = format!("{}/.zshrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "shell config is not a secret");
}

#[test]
fn allows_app_config_under_config_dir() {
    let path = format!("{}/.config/app/settings.json", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "generic app config is not a secret");
}

// --- node_modules exception ----------------------------------------------

#[test]
fn allows_reading_from_node_modules_in_nvm() {
    let path = format!(
        "{}/.nvm/versions/node/v20.0.0/lib/node_modules/@types/node/index.d.ts",
        home()
    );
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "node_modules under .nvm is readable",
    );
}

#[test]
fn allows_reading_pi_documentation_from_node_modules() {
    let path = format!(
        "{}/.nvm/versions/node/v24.14.1/lib/node_modules/@earendil-works/pi-coding-agent/docs/providers.md",
        home()
    );
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "node_modules under .nvm is readable",
    );
}

// --- hidden files in CWD --------------------------------------------------

#[test]
fn allows_dot_env_in_cwd() {
    let result = check_read(".env", &config::default_config());
    assert_action(&result, Action::Allow, "hidden files in CWD read freely");
}

#[test]
fn allows_gitignore_in_cwd() {
    let result = check_read(".gitignore", &config::default_config());
    assert_action(&result, Action::Allow, "hidden files in CWD read freely");
}

// --- system directories ---------------------------------------------------

#[test]
fn allows_reading_etc_passwd() {
    let result = check_read("/etc/passwd", &config::default_config());
    // Read default is allow; only secret locations in HOME ask.
    assert_action(&result, Action::Allow, "read default is allow");
}
