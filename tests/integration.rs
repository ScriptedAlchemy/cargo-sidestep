use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = env::temp_dir().join(format!("cargo-sidestep-{name}-{stamp}"));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn reroutes_package_cache_locks_into_online_overlay() {
    let root = temp_dir("package-cache");
    let attempts = root.join("attempts.txt");
    let state_dir = root.join("state");
    let base_home = root.join("base-home");
    fs::create_dir_all(base_home.join("registry").join("cache")).unwrap();
    fs::write(
        base_home.join("config.toml"),
        "[net]\ngit-fetch-with-cli = true\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-sidestep"))
        .arg("build")
        .env("CARGO_SIDESTEP_CARGO_BIN", "./tests/fake_cargo.sh")
        .env("FAKE_CARGO_ATTEMPTS_FILE", &attempts)
        .env("FAKE_CARGO_MODE", "package-cache")
        .env("FAKE_BASE_CARGO_HOME", &base_home)
        .env("CARGO_HOME", &base_home)
        .env("CARGO_SIDESTEP_STATE_DIR", &state_dir)
        .env("CARGO_SIDESTEP_FALLBACK_AFTER_MS", "100")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("plan=online-overlay"), "{stdout}");
    assert!(stdout.contains("subcommand=build"), "{stdout}");
    assert!(stdout.contains("cargo_home="), "{stdout}");
    let attempts_count = fs::read_to_string(&attempts).unwrap();
    assert_eq!(attempts_count.trim(), "3");
}

#[test]
fn reroutes_build_directory_locks_into_a_lane() {
    let root = temp_dir("build-dir");
    let attempts = root.join("attempts.txt");
    let state_dir = root.join("state");
    let base_home = root.join("base-home");
    fs::create_dir_all(&base_home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-sidestep"))
        .arg("check")
        .env("CARGO_SIDESTEP_CARGO_BIN", "./tests/fake_cargo.sh")
        .env("FAKE_CARGO_ATTEMPTS_FILE", &attempts)
        .env("FAKE_CARGO_MODE", "build-dir")
        .env("FAKE_BASE_CARGO_HOME", &base_home)
        .env("CARGO_HOME", &base_home)
        .env("CARGO_SIDESTEP_STATE_DIR", &state_dir)
        .env("CARGO_SIDESTEP_FALLBACK_AFTER_MS", "100")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("plan=build-lane"), "{stdout}");
    assert!(stdout.contains("subcommand=check"), "{stdout}");
    assert!(stdout.contains("build_dir="), "{stdout}");
}

#[test]
fn strips_plugin_prefix_when_invoked_as_cargo_subcommand() {
    let root = temp_dir("plugin-prefix");
    let attempts = root.join("attempts.txt");
    let state_dir = root.join("state");
    let base_home = root.join("base-home");
    fs::create_dir_all(&base_home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-sidestep"))
        .arg("sidestep")
        .arg("check")
        .env("CARGO_SIDESTEP_CARGO_BIN", "./tests/fake_cargo.sh")
        .env("FAKE_CARGO_ATTEMPTS_FILE", &attempts)
        .env("FAKE_CARGO_MODE", "none")
        .env("FAKE_BASE_CARGO_HOME", &base_home)
        .env("CARGO_HOME", &base_home)
        .env("CARGO_SIDESTEP_STATE_DIR", &state_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("subcommand=check"), "{stdout}");
    assert!(!stdout.contains("subcommand=sidestep"), "{stdout}");
}
