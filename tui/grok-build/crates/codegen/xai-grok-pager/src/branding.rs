//! Product branding.
//!
//! OpenBuild reuses the grok-build TUI, so every user-visible product name has
//! to come from one place. Keeping the strings here means a rebrand is a single
//! file change, and it keeps the upstream identifiers (crate names, env vars,
//! on-disk paths such as `~/.grok`) untouched — those are compatibility
//! surfaces, not branding.

/// Display name shown in the version badge, hero box, window title and prompts.
pub const PRODUCT_NAME: &str = "OpenBuild";

/// Short name used where space is tight (tab titles, notification titles).
pub const PRODUCT_SHORT: &str = "OpenBuild";

/// CLI binary name used in user-facing copy (`/doctor`, error hints, docs).
pub const CLI_NAME: &str = "openbuild";

/// Kernel executable name, used in setup hints.
pub const KERNEL_NAME: &str = "openbuild-kernel";

/// Hero subtitle on the welcome screen.
pub const HERO_SUBTITLE: &str = "Thanks for trying OpenBuild, give feedback with /feedback!";

/// Text the backend is referred to by in settings and diagnostics.
pub const BACKEND_LABEL: &str = "OpenBuild kernel";

/// One-line description for `--help` / about.
pub const ABOUT: &str = "OpenBuild TUI";

/// Rendered as `"{PRODUCT_NAME}  "` in the version badge.
pub fn badge_name() -> &'static str {
    PRODUCT_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_name_is_openbuild() {
        assert_eq!(PRODUCT_NAME, "OpenBuild");
        assert_eq!(CLI_NAME, "openbuild");
        assert_eq!(KERNEL_NAME, "openbuild-kernel");
    }

    #[test]
    fn no_upstream_branding_leaks_into_user_copy() {
        for text in [
            PRODUCT_NAME,
            PRODUCT_SHORT,
            CLI_NAME,
            KERNEL_NAME,
            HERO_SUBTITLE,
            BACKEND_LABEL,
            ABOUT,
        ] {
            assert!(
                !text.to_ascii_lowercase().contains("grok"),
                "branding string still mentions grok: {text}"
            );
            assert!(
                !text.to_ascii_lowercase().contains("opencode"),
                "branding string still mentions opencode: {text}"
            );
        }
    }
}
