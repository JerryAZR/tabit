//! Secret-path scenarios (ported from pi-sanity
//! `tests/integration/scenarios/hidden-files.test.ts`): known
//! credential locations ask, non-secret dotfiles allow, node_modules
//! is readable, and hidden files in CWD are fair game for all three
//! checkers.

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

use common::{assert_action, home};
use tabit_gate::checker_bash::check_bash;
use tabit_gate::checker_read::check_read;
use tabit_gate::checker_write::check_write;
use tabit_gate::config;
use tabit_gate::types::Action;

// --- known credential locations ---------------------------------------------

#[test]
fn asks_for_aws_credentials() {
    let path = format!("{}/.aws/credentials", home());
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "~/.aws/credentials may contain secrets",
    );
}

#[test]
fn asks_for_private_ssh_key() {
    let path = format!("{}/.ssh/id_rsa", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Ask, "~/.ssh/id_rsa may contain secrets");
}

#[test]
fn asks_for_netrc() {
    let path = format!("{}/.netrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Ask, "~/.netrc may contain secrets");
}

#[test]
fn asks_for_kube_config() {
    let path = format!("{}/.kube/config", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Ask, "~/.kube/config may contain secrets");
}

#[test]
fn asks_for_docker_config() {
    let path = format!("{}/.docker/config.json", home());
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "~/.docker/config.json may contain secrets",
    );
}

#[test]
fn asks_for_npmrc() {
    let path = format!("{}/.npmrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Ask, "~/.npmrc may contain secrets");
}

// --- safe paths that should be allowed ----------------------------------------

#[test]
fn allows_bashrc_as_non_secret() {
    let path = format!("{}/.bashrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "shell config is not a secret");
}

#[test]
fn allows_zshrc_as_non_secret() {
    let path = format!("{}/.zshrc", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "shell config is not a secret");
}

#[test]
fn allows_cargo_directory() {
    let path = format!("{}/.cargo", home());
    let result = check_read(&path, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "the Rust crate cache is not a secret",
    );
}

#[test]
fn allows_generic_app_config_under_config_dir() {
    let path = format!("{}/.config/app/settings.json", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "generic app config is not a secret");
}

#[test]
fn allows_files_under_cache() {
    let path = format!("{}/.cache/npm/content/file", home());
    let result = check_read(&path, &config::default_config());
    assert_action(&result, Action::Allow, "caches are not secrets");
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

// --- npm package paths (allowed for docs/types) --------------------------------

#[test]
fn allows_reading_from_nvm_node_modules() {
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

// --- hidden files in CWD (allowed) ----------------------------------------------

#[test]
fn allows_dot_env_read() {
    let result = check_read(".env", &config::default_config());
    assert_action(&result, Action::Allow, "hidden CWD files read freely");
}

#[test]
fn allows_dot_env_write() {
    let result = check_write(".env", &config::default_config());
    assert_action(&result, Action::Allow, "hidden CWD files write freely");
}

#[test]
fn allows_gitignore_write() {
    let result = check_write(".gitignore", &config::default_config());
    assert_action(&result, Action::Allow, "hidden CWD files write freely");
}

#[test]
fn allows_rm_dot_env() {
    let result = check_bash("rm .env", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "deleting hidden CWD files is allowed",
    );
}

#[test]
fn allows_rm_prettierrc() {
    let result = check_bash("rm .prettierrc", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "deleting hidden CWD files is allowed",
    );
}

// --- bash scenarios with hidden files ---------------------------------------------

#[test]
fn allows_echo_into_dot_env() {
    let result = check_bash("echo API_KEY=secret > .env", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "redirecting into hidden CWD files is allowed",
    );
}

#[test]
fn allows_echo_into_gitignore() {
    let result = check_bash("echo node_modules/ > .gitignore", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "redirecting into hidden CWD files is allowed",
    );
}
