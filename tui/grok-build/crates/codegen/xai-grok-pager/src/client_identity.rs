//! ACP client identity sent in `initialize`.
//!
//! These strings are wire-level identifiers, not branding: the kernel may key
//! behaviour off them, so they stay stable across rebrands.

pub const PAGER_CLIENT_TYPE: &str = "openbuild-tui";
pub const HEADLESS_CLIENT_TYPE: &str = "openbuild-agent";

pub const PAGER_CLIENT_VERSION: &str = xai_grok_version::VERSION;

/// `User-Agent` for the pager's own HTTP clients that call `api.x.ai` directly (voice STT).
///
/// Matches the sampler's `<client>/<version> (os; arch)` shape so server-side dashboards
/// bucket this traffic alongside chat / imagine requests.
pub fn client_user_agent() -> String {
    format!(
        "{}/{} ({}; {})",
        HEADLESS_CLIENT_TYPE,
        PAGER_CLIENT_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_user_agent_has_expected_shape() {
        // e.g. "openbuild-agent/1.2.3 (macos; aarch64)".
        // Servers parse this UA string, so pin the exact shape
        let ua = client_user_agent();
        assert_eq!(
            ua,
            format!(
                "{}/{} ({}; {})",
                HEADLESS_CLIENT_TYPE,
                PAGER_CLIENT_VERSION,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        );
    }

    #[test]
    fn client_types_are_not_upstream_branded() {
        for value in [PAGER_CLIENT_TYPE, HEADLESS_CLIENT_TYPE] {
            let lower = value.to_ascii_lowercase();
            assert!(!lower.contains("grok"), "{value}");
            assert!(!lower.contains("opencode"), "{value}");
        }
    }
}
