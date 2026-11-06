//! Classifier coverage: every command family, flags and chain handling.

use std::{fs, path::Path};

use oc_lanes::classify;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture");
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonical fixture")
        .to_string_lossy()
        .into_owned()
}

fn lane_for(command: &str, cwd: &Path) -> String {
    classify(command, cwd)
        .unwrap_or_else(|| panic!("`{command}` was not classified"))
        .lane
}

#[test]
fn cargo_family_and_flags() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(temp.path(), "Cargo.toml", "[workspace]\nmembers = []\n");
    write(
        temp.path(),
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\n",
    );
    write(temp.path(), "crates/app/src/main.rs", "fn main() {}\n");
    let cwd = temp.path().join("crates/app/src");
    let expected = format!("cargo:{}", canonical(temp.path()));

    for command in [
        "cargo build",
        "cargo test --workspace",
        "cargo clippy --all-targets -- -D warnings",
        "cargo nextest run --no-fail-fast",
        "cargo run --release",
        "cargo check",
        "cargo +nightly test",
        "cargo --offline test",
        "cargo --manifest-path Cargo.toml test",
        "rustc src/main.rs",
        "CARGO_TARGET_DIR=/tmp/target cargo test",
        "env FOO=1 cargo test",
        "time -p cargo test",
    ] {
        let request = classify(command, &cwd).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(request.lane, expected, "{command}");
        assert_eq!(request.command, command, "{command}");
    }
}

#[test]
fn cargo_package_root_when_no_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(
        temp.path(),
        "project/Cargo.toml",
        "[package]\nname = \"project\"\n",
    );
    write(temp.path(), "project/src/lib.rs", "");
    let cwd = temp.path().join("project/src");
    assert_eq!(
        lane_for("cargo test", &cwd),
        format!("cargo:{}", canonical(&temp.path().join("project")))
    );
}

#[test]
fn cargo_nearest_workspace_beats_package() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(
        temp.path(),
        "Cargo.toml",
        "[workspace.package]\nversion = \"0.1.0\"\n",
    );
    write(
        temp.path(),
        "member/Cargo.toml",
        "[package]\nname = \"member\"\n",
    );
    let cwd = temp.path().join("member");
    assert_eq!(
        lane_for("cargo test", &cwd),
        format!("cargo:{}", canonical(temp.path()))
    );
}

#[test]
fn node_family_and_flags() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(temp.path(), "package.json", "{\"name\":\"app\"}\n");
    write(temp.path(), "packages/web/package.json", "{}\n");
    let cwd = temp.path().join("packages/web");
    let expected = format!("node:{}", canonical(&cwd));

    for command in [
        "npm test",
        "npm run test",
        "npm run build -- --production",
        "pnpm test",
        "pnpm run build",
        "bun test",
        "bun run check",
        "yarn test",
        "yarn run lint",
        "npm --prefix . test",
        "pnpm -C . build",
    ] {
        assert_eq!(lane_for(command, &cwd), expected, "{command}");
    }
    assert!(classify("npm install", &cwd).is_none());
    assert!(classify("npm run", &cwd).is_none());
    assert!(classify("yarn add left-pad", &cwd).is_none());
}

#[test]
fn non_cargo_families() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path();
    let root = canonical(cwd);
    for (command, prefix) in [
        ("go test ./...", "go"),
        ("go -C . test ./pkg", "go"),
        ("pytest -q tests", "python"),
        ("python -m pytest tests", "python"),
        ("python3 -m py.test", "python"),
        ("dotnet test --no-restore", "dotnet"),
        ("gradle test", "gradle"),
        ("./gradlew build", "gradle"),
        ("mvn test", "maven"),
        ("./mvnw package", "maven"),
    ] {
        assert_eq!(
            lane_for(command, cwd),
            format!("{prefix}:{root}"),
            "{command}"
        );
    }
}

#[test]
fn unrelated_commands_are_not_classified() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path();
    for command in [
        "echo hello",
        "git status",
        "cargo fmt",
        "cargo metadata",
        "go build ./...",
        "python script.py",
        "dotnet build",
        "gradle --version",
        "mvn --version",
    ] {
        if command == "gradle --version" || command == "mvn --version" {
            // Maven and Gradle invocations hold their build directory even
            // when they only print version information.
            assert!(classify(command, cwd).is_some(), "{command}");
        } else {
            assert!(classify(command, cwd).is_none(), "{command}");
        }
    }
}

#[test]
fn chains_use_the_first_recognized_segment() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(temp.path(), "Cargo.toml", "[workspace]\n");
    write(temp.path(), "package.json", "{}");
    let cwd = temp.path();
    let cargo = format!("cargo:{}", canonical(cwd));
    let node = format!("node:{}", canonical(cwd));

    assert_eq!(
        lane_for("cargo fmt --check && cargo test --workspace", cwd),
        cargo
    );
    assert_eq!(lane_for("cd packages/app && npm test", cwd), node);
    assert_eq!(lane_for("npm install && npm run build", cwd), node);
    assert_eq!(lane_for("true; cargo clippy", cwd), cargo);
    assert_eq!(lane_for("false || cargo nextest run", cwd), cargo);
    assert!(classify("echo \"a && b\"", cwd).is_none());
    assert!(classify("npm install && git status", cwd).is_none());
}

#[test]
fn missing_manifest_falls_back_to_canonical_cwd() {
    let temp = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        lane_for("cargo test", temp.path()),
        format!("cargo:{}", canonical(temp.path()))
    );
    assert_eq!(
        lane_for("pytest", temp.path()),
        format!("python:{}", canonical(temp.path()))
    );
}

#[test]
fn cd_prefix_and_manifest_path_resolve_target_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(temp.path(), "host/Cargo.toml", "[workspace]\n");
    write(temp.path(), "target_repo/Cargo.toml", "[workspace]\n");
    let host = temp.path().join("host");
    let target_repo = temp.path().join("target_repo");
    let expected = format!("cargo:{}", canonical(&target_repo));

    assert_eq!(
        lane_for(
            &format!("cd {} && cargo test --workspace", target_repo.display()),
            &host
        ),
        expected
    );
    assert_eq!(
        lane_for(
            &format!(
                "cargo test --manifest-path {}/Cargo.toml",
                target_repo.display()
            ),
            &host
        ),
        expected
    );
}

#[cfg(windows)]
#[test]
fn windows_paths_and_executables() {
    use std::path::PathBuf;

    let temp = tempfile::tempdir().expect("tempdir");
    write(temp.path(), "Cargo.toml", "[workspace]\n");
    let expected = format!("cargo:{}", canonical(temp.path()));

    let quoted = r#""C:\Program Files\cargo\bin\cargo.exe" test --workspace"#;
    assert_eq!(lane_for(quoted, temp.path()), expected);

    let missing = PathBuf::from(r"C:\Users\dev\project");
    assert_eq!(
        lane_for("cargo test", &missing),
        format!("cargo:{}", missing.display())
    );
}
