use std::sync::{Arc, OnceLock};

static CHROME_TEST_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

/// Set by PR CI, which leaves real-browser fixtures to the nightly `chrome`
/// nextest profile.
pub(crate) const FORBID_REAL_CHROME_ENV: &str = "CCCC_TEST_FORBID_REAL_CHROME";

/// Set by fork-ci's strict-browser job (`require_browser=true`): every
/// real-browser fixture must run, so a missing prerequisite fails the test
/// instead of letting it report ok without exercising a browser.
const REQUIRE_BROWSER_ENV: &str = "REQUIRE_BROWSER";

fn strict_browser_required_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn strict_browser_required() -> bool {
    strict_browser_required_value(std::env::var(REQUIRE_BROWSER_ENV).ok().as_deref())
}

fn refuse_browser_fixture_skip(strict: bool, prerequisite: &str) {
    assert!(
        !strict,
        "strict browser fixture refused: {prerequisite} is unavailable"
    );
}

/// Call inside a real-browser fixture's early-return branch. Normally the
/// fixture skips; under REQUIRE_BROWSER the missing prerequisite is a failure,
/// so a gated check refuses instead of passing silently.
pub(crate) fn browser_fixture_skip(prerequisite: &str) {
    refuse_browser_fixture_skip(strict_browser_required(), prerequisite);
}

// Shared by surface, prompt and route tests. Keep the guard until all fixture
// browsers are closed to limit cold-launch contention on small CI runners.
pub(crate) async fn chrome_test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    assert!(
        std::env::var_os(FORBID_REAL_CHROME_ENV).is_none(),
        "this test launches a real Chrome, which PR CI does not run; add it to the \
         real-Chrome filter in .config/nextest.toml so the nightly job covers it"
    );
    Arc::clone(CHROME_TEST_LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
        .lock_owned()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_browser_required_reads_truthy_env_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            assert!(strict_browser_required_value(Some(value)), "{value}");
        }
        for value in [
            None,
            Some(""),
            Some("false"),
            Some("0"),
            Some("no"),
            Some("off"),
        ] {
            assert!(!strict_browser_required_value(value), "{value:?}");
        }
    }

    #[test]
    fn ordinary_mode_still_allows_a_browser_fixture_skip() {
        refuse_browser_fixture_skip(false, "a system browser");
    }

    #[test]
    #[should_panic(expected = "strict browser fixture refused")]
    fn strict_mode_turns_a_missing_prerequisite_into_a_failure() {
        refuse_browser_fixture_skip(true, "a system browser");
    }
}
