use std::sync::{Arc, OnceLock};

static CHROME_TEST_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

/// Set by PR CI, which leaves real-browser fixtures to the nightly `chrome`
/// nextest profile.
pub(crate) const FORBID_REAL_CHROME_ENV: &str = "CCCC_TEST_FORBID_REAL_CHROME";

/// Set by a gated run that must execute real-browser fixtures (for example a
/// strict-browser CI job). With it set, a fixture that finds no browser — or
/// no display server — fails instead of silently returning early.
pub(crate) const REQUIRE_BROWSER_ENV: &str = "CCCC_TEST_REQUIRE_BROWSER";

/// Call inside every real-browser fixture's no-browser early return, before
/// the `return`. Without [`REQUIRE_BROWSER_ENV`] this is a no-op and the
/// fixture keeps skipping; with it set the fixture fails, so a gated check
/// refuses rather than skips.
#[track_caller]
pub(crate) fn refuse_silent_browser_skip() {
    assert!(
        std::env::var_os(REQUIRE_BROWSER_ENV).is_none(),
        "{REQUIRE_BROWSER_ENV} is set but the fixture's browser/display \
         prerequisites are absent; refusing to pass without executing it"
    );
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
