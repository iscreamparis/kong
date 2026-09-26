use super::*;
fn fixture() -> Vec<GhRelease> {
    serde_json::from_str(include_str!("fixtures/python-releases.json")).unwrap()
}
#[test]
fn platform_filter_and_empty_releases_fail_clearly() {
    let error = select_asset(&fixture(), "3.11", "aarch64-apple-darwin-install_only")
        .unwrap_err()
        .to_string();
    assert!(error.contains("available versions in scanned releases: none"));
    assert!(select_asset(&[], "3.11", PLATFORM)
        .unwrap_err()
        .to_string()
        .contains("3.11"));
    assert!(select_asset(&fixture(), "../3.11", PLATFORM).is_err());
}
const PLATFORM: &str = "x86_64-unknown-linux-gnu-install_only";
#[test]
fn requested_minor_selects_highest_patch_not_first_asset() {
    assert_eq!(
        select_asset(&fixture(), "3.11", PLATFORM).unwrap().1,
        "3.11.14"
    );
}
#[test]
fn exact_patch_prefers_exact_in_older_release() {
    assert_eq!(
        select_asset(&fixture(), "3.11.9", PLATFORM).unwrap().1,
        "3.11.9"
    );
}
#[test]
fn absent_patch_falls_back_only_within_minor() {
    assert_eq!(
        select_asset(&fixture(), "3.11.99", PLATFORM).unwrap().1,
        "3.11.14"
    );
}
#[test]
fn unavailable_minor_names_request_and_available_versions() {
    let error = select_asset(&fixture(), "3.1", PLATFORM)
        .unwrap_err()
        .to_string();
    for text in ["3.1", "3.10.21", "3.11.14", "available"] {
        assert!(error.contains(text), "{error}");
    }
}
#[test]
fn latest_preserves_first_matching_asset() {
    for request in ["", "latest"] {
        assert_eq!(
            select_asset(&fixture(), request, PLATFORM).unwrap().1,
            "3.10.21"
        );
    }
}
