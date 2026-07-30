const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

fn job(name: &str, next_name: Option<&str>) -> &'static str {
    let start_marker = format!("\n  {name}:\n");
    let start = WORKFLOW
        .find(&start_marker)
        .unwrap_or_else(|| panic!("release workflow must define job {name:?}"));
    let body_start = start + 1;

    match next_name {
        Some(next_name) => {
            let end_marker = format!("\n  {next_name}:\n");
            let relative_end = WORKFLOW[body_start..].find(&end_marker).unwrap_or_else(|| {
                panic!("release workflow must define job {next_name:?} after {name:?}")
            });
            &WORKFLOW[body_start..body_start + relative_end]
        }
        None => &WORKFLOW[body_start..],
    }
}

fn assert_contains_all(section_name: &str, section: &str, required: &[&str]) {
    for value in required {
        assert!(
            section.contains(value),
            "{section_name} must contain {value:?}"
        );
    }
}

#[test]
fn workflow_scopes_static_msvc_rustflags_to_windows_jobs() {
    let windows_check = job("build-and-check", Some("unix-build-and-check"));
    let unix_checks = job("unix-build-and-check", Some("release-candidate"));
    let windows_candidate = job("release-candidate", Some("unix-release-candidate"));
    let unix_candidates = job("unix-release-candidate", Some("publish-release"));
    let static_msvc_rustflags = "RUSTFLAGS: \"-C target-feature=+crt-static\"";

    assert!(
        !WORKFLOW.contains("\nenv:\n  RUSTFLAGS:"),
        "Windows-only Rust flags must not be defined globally"
    );
    assert_contains_all("build-and-check", windows_check, &[static_msvc_rustflags]);
    assert_contains_all(
        "release-candidate",
        windows_candidate,
        &[static_msvc_rustflags],
    );
    assert!(!unix_checks.contains(static_msvc_rustflags));
    assert!(!unix_candidates.contains(static_msvc_rustflags));
}

#[test]
fn pull_request_checks_cover_linux_x86_64_and_macos_aarch64_independently() {
    let checks = job("unix-build-and-check", Some("release-candidate"));

    assert_contains_all(
        "unix-build-and-check",
        checks,
        &[
            "fail-fast: false",
            "runs-on: ${{ matrix.os }}",
            "os: ubuntu-22.04",
            "target: x86_64-unknown-linux-gnu",
            "os: macos-14",
            "target: aarch64-apple-darwin",
            "rustup toolchain install",
            "cargo test --locked --target",
            "--all-features",
        ],
    );
}

#[test]
fn unix_release_candidates_have_stable_names_and_verify_their_archives() {
    let candidates = job("unix-release-candidate", Some("publish-release"));

    assert_contains_all(
        "unix-release-candidate",
        candidates,
        &[
            "fail-fast: false",
            "target: x86_64-unknown-linux-gnu",
            "archive_name: onec-buddy-mcp-linux-x86_64.tar.gz",
            "target: aarch64-apple-darwin",
            "archive_name: onec-buddy-mcp-macos-aarch64.tar.gz",
            "cargo build --release --locked --target",
            "tar -czf",
            "tar -tzf",
            "shasum -a 256",
            "RELEASE_METADATA",
            "actions/upload-artifact@",
        ],
    );
}

#[test]
fn manual_feature_branch_builds_candidates_without_publishing_a_release() {
    let identity = job("release-version", Some("build-and-check"));
    let windows_candidate = job("release-candidate", Some("unix-release-candidate"));
    let unix_candidates = job("unix-release-candidate", Some("publish-release"));
    let publication = job("publish-release", None);

    for (name, candidates) in [
        ("release-candidate", windows_candidate),
        ("unix-release-candidate", unix_candidates),
    ] {
        assert!(
            candidates.contains("if: github.event_name == 'workflow_dispatch'"),
            "{name} must run for a manual build from any branch"
        );
        assert!(
            !candidates.contains("github.ref_name == 'main'"),
            "{name} must not be limited to the main branch"
        );
    }

    assert!(
        !identity.contains("Releases can be started only from the main branch"),
        "a manual build-only run must be allowed outside the main branch"
    );
    assert_contains_all(
        "publish-release",
        publication,
        &[
            "if: github.event_name == 'workflow_dispatch'",
            "github.ref_name == 'main'",
            "gh release create",
        ],
    );
}

#[test]
fn publication_requires_and_verifies_all_three_platform_packages() {
    let publication = job("publish-release", None);

    assert_contains_all(
        "publish-release",
        publication,
        &[
            "- release-candidate",
            "- unix-release-candidate",
            "onec-buddy-mcp-windows-x86_64.zip",
            "onec-buddy-mcp-linux-x86_64.tar.gz",
            "onec-buddy-mcp-macos-aarch64.tar.gz",
            "SHA256SUMS.txt",
            "Get-FileHash",
            "gh release create",
        ],
    );
}
