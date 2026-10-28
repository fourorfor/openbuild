//! Proxy discovery for the kernel process.
//!
//! The kernel performs every model request, and Bun's `fetch` reads
//! `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` from its environment — but only
//! when those variables are actually set. A window launched from Explorer, a
//! service-spawned kernel, or a terminal opened before the variables existed
//! all miss them, and Bun deliberately ignores the Windows system (WinINET)
//! proxy. The result is a kernel that quietly connects directly.
//!
//! This module resolves one answer for the kernel:
//!
//! 1. `OPENBUILD_PROXY` — the explicit switch, including `off`/`none`/`direct`
//!    to force direct connections.
//! 2. The ambient `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` variables.
//! 3. The Windows system proxy (WinINET, per-user).
//! 4. Nothing: the kernel keeps the behaviour it would have without us.
//!
//! Whatever a source yields is probed first: a proxy that refuses connections
//! is dropped so the kernel goes direct instead of failing every request. The
//! probe only proves the port accepts a connection; a reachable proxy that is
//! broken upstream is still a live risk, which is why the kernel retries a
//! failed request directly (see `net/proxy.ts` in the vendored kernel).
//!
//! The resolved proxy is handed to the kernel as environment variables *and*
//! as `OPENBUILD_PROXY`, so the kernel knows the direct fallback is armed.

use std::fmt;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Environment variable carrying the explicit decision to the kernel.
pub const PROXY_ENV: &str = "OPENBUILD_PROXY";

/// How long a proxy port may take to accept a connection.
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Where a resolved proxy came from, for `/backend`-style reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    /// `OPENBUILD_PROXY` was set to a proxy URL.
    Explicit,
    /// `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` supplied it.
    Environment,
    /// The Windows system proxy supplied it.
    System,
}

impl ProxySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Explicit => "OPENBUILD_PROXY",
            Self::Environment => "environment",
            Self::System => "system",
        }
    }
}

/// A usable proxy address such as `http://127.0.0.1:10808`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    /// Normalised URL, always with a scheme.
    pub url: String,
    pub source: ProxySource,
}

impl Proxy {
    /// Host and port for the reachability probe.
    fn endpoint(&self) -> Option<(String, u16)> {
        let rest = self.url.split_once("://")?.1;
        let authority = rest.split('/').next()?;
        // Strip credentials: `user:pass@host:port`.
        let authority = authority.rsplit('@').next()?;
        // IPv6 literals keep their brackets in a URL but not in `to_socket_addrs`.
        if let Some(inner) = authority.strip_prefix('[') {
            let (host, tail) = inner.split_once(']')?;
            let port = tail.strip_prefix(':').and_then(|value| value.parse().ok());
            return Some((host.to_string(), port.unwrap_or(8080)));
        }
        match authority.rsplit_once(':') {
            Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
            None => Some((authority.to_string(), 8080)),
        }
    }
}

impl fmt::Display for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.url, self.source.label())
    }
}

/// Why no proxy is in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Use this proxy.
    Use(Proxy),
    /// Explicitly disabled by `OPENBUILD_PROXY`.
    Disabled,
    /// Nothing configured anywhere.
    Unconfigured,
    /// Configured, but the port refused a connection.
    Unreachable(String),
}

/// Resolve the proxy the kernel should use.
///
/// `lookup` reads an environment variable; `system` returns the Windows system
/// proxy. `probe` reports whether host:port accepts a connection. Splitting
/// them out keeps the decision testable without touching the real machine.
pub fn resolve(
    lookup: impl Fn(&str) -> Option<String>,
    system: impl Fn() -> Option<String>,
    probe: impl FnMut(&str, u16) -> bool,
) -> Decision {
    if let Some(raw) = lookup(PROXY_ENV) {
        let value = raw.trim();
        if is_direct_switch(value) {
            return Decision::Disabled;
        }
        if !value.is_empty() {
            return check(
                Proxy {
                    url: normalise(value),
                    source: ProxySource::Explicit,
                },
                probe,
            );
        }
    }

    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Some(raw) = lookup(name) {
            let value = raw.trim();
            if !value.is_empty() && !is_direct_switch(value) {
                return check(
                    Proxy {
                        url: normalise(value),
                        source: ProxySource::Environment,
                    },
                    probe,
                );
            }
        }
    }

    if let Some(raw) = system() {
        let value = raw.trim();
        if !value.is_empty() {
            return check(
                Proxy {
                    url: normalise(value),
                    source: ProxySource::System,
                },
                probe,
            );
        }
    }

    Decision::Unconfigured
}

fn check(proxy: Proxy, mut probe: impl FnMut(&str, u16) -> bool) -> Decision {
    match proxy.endpoint() {
        Some((host, port)) if probe(&host, port) => Decision::Use(proxy),
        Some(_) => Decision::Unreachable(proxy.url),
        None => Decision::Unreachable(proxy.url),
    }
}

fn is_direct_switch(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "off" | "none" | "direct" | "false" | "0"
    )
}

/// Give a bare `host:port` an `http://` scheme; keep any scheme as given.
pub fn normalise(value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    }
}

/// Read the current process environment.
pub fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The Windows system (WinINET) proxy, if enabled.
///
/// Reads `HKCU\...\Internet Settings` through `reg.exe`: the TUI already ships
/// without a registry crate, and one short-lived process at startup is cheaper
/// than adding a dependency for two values.
#[cfg(windows)]
pub fn system_proxy() -> Option<String> {
    let enabled = reg_value("ProxyEnable").unwrap_or_default();
    if enabled.trim() != "1" {
        return None;
    }
    let server = reg_value("ProxyServer")?;
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    // `ProxyServer` is either `host:port` or a per-scheme list such as
    // `http=host:port;https=host:port`. Prefer the HTTPS entry.
    let mut chosen = None;
    for part in server.split(';') {
        let (name, value) = match part.split_once('=') {
            Some((name, value)) => (Some(name.trim().to_ascii_lowercase()), value.trim()),
            None => (None, part.trim()),
        };
        if value.is_empty() {
            continue;
        }
        match name.as_deref() {
            Some("https") => return Some(normalise(value)),
            _ => {
                chosen.get_or_insert_with(|| normalise(value));
            }
        }
    }
    chosen
}

#[cfg(windows)]
fn reg_value(name: &str) -> Option<String> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let output = std::process::Command::new("reg")
        .args(["query", KEY, "/v", name])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if !fields.next()?.eq_ignore_ascii_case(name) {
            return None;
        }
        // `reg` prints the type between the name and the value: `REG_SZ` for
        // strings, `REG_DWORD` for numbers (`0x1`). Both shapes are used by
        // `Internet Settings`, so neither may be assumed.
        let kind = fields.next()?;
        let raw = fields.collect::<Vec<_>>().join(" ");
        if raw.is_empty() {
            return None;
        }
        if kind.eq_ignore_ascii_case("REG_DWORD") {
            let value = raw.trim_start_matches("0x");
            return Some(u64::from_str_radix(value, 16).ok()?.to_string());
        }
        Some(raw)
    })
}

#[cfg(not(windows))]
pub fn system_proxy() -> Option<String> {
    None
}

/// Does `host:port` accept a TCP connection within [`PROBE_TIMEOUT`]?
pub fn probe(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .map(SocketAddr::from)
        .any(|addr| TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok())
}

/// Resolve against the real machine.
pub fn detect() -> Decision {
    resolve(env_lookup, system_proxy, probe)
}

/// The environment additions that make the kernel obey `decision`.
///
/// A non-proxy decision actively clears the ambient variables: otherwise a
/// stale `HTTPS_PROXY` from the parent shell would keep overriding the choice.
pub fn kernel_env(decision: &Decision) -> Vec<(String, String)> {
    match decision {
        Decision::Use(proxy) => vec![
            ("HTTPS_PROXY".to_string(), proxy.url.clone()),
            ("HTTP_PROXY".to_string(), proxy.url.clone()),
            ("ALL_PROXY".to_string(), proxy.url.clone()),
            (PROXY_ENV.to_string(), proxy.url.clone()),
        ],
        Decision::Disabled | Decision::Unconfigured | Decision::Unreachable(_) => vec![
            ("HTTPS_PROXY".to_string(), String::new()),
            ("HTTP_PROXY".to_string(), String::new()),
            ("ALL_PROXY".to_string(), String::new()),
            (PROXY_ENV.to_string(), "off".to_string()),
        ],
    }
}

/// One-line status for the UI.
pub fn describe(decision: &Decision) -> String {
    match decision {
        Decision::Use(proxy) => format!("proxy {proxy}"),
        Decision::Disabled => "proxy off (requested)".to_string(),
        Decision::Unconfigured => "proxy auto (none found)".to_string(),
        Decision::Unreachable(url) => format!("proxy auto ({url} unreachable)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the real machine: environment, Windows registry, and a live probe.
    /// Ignored by default because the result depends on the host.
    #[test]
    #[ignore]
    fn detect_reports_the_real_machine() {
        println!("detect() = {}", describe(&detect()));
    }

    fn env_of(pairs: &[(&str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn explicit_proxy_wins_over_the_environment() {
        let decision = resolve(
            env_of(&[
                (PROXY_ENV, "http://127.0.0.1:10808"),
                ("HTTPS_PROXY", "http://127.0.0.1:9999"),
            ]),
            || None,
            |_, _| true,
        );
        match decision {
            Decision::Use(proxy) => {
                assert_eq!(proxy.url, "http://127.0.0.1:10808");
                assert_eq!(proxy.source, ProxySource::Explicit);
            }
            other => panic!("expected the explicit proxy, got {other:?}"),
        }
    }

    #[test]
    fn explicit_off_disables_a_configured_environment_proxy() {
        let decision = resolve(
            env_of(&[
                (PROXY_ENV, "off"),
                ("HTTPS_PROXY", "http://127.0.0.1:10808"),
            ]),
            || Some("http://127.0.0.1:7890".to_string()),
            |_, _| true,
        );
        assert_eq!(decision, Decision::Disabled);
    }

    #[test]
    fn environment_proxy_is_used_when_explicit_is_absent() {
        let decision = resolve(
            env_of(&[("HTTPS_PROXY", "http://127.0.0.1:10808")]),
            || None,
            |host, port| host == "127.0.0.1" && port == 10808,
        );
        match decision {
            Decision::Use(proxy) => {
                assert_eq!(proxy.source, ProxySource::Environment);
                assert_eq!(proxy.url, "http://127.0.0.1:10808");
            }
            other => panic!("expected the environment proxy, got {other:?}"),
        }
    }

    #[test]
    fn system_proxy_is_the_last_resort() {
        let decision = resolve(env_of(&[]), || Some("127.0.0.1:7890".to_string()), |_, _| {
            true
        });
        match decision {
            Decision::Use(proxy) => {
                assert_eq!(proxy.source, ProxySource::System);
                assert_eq!(proxy.url, "http://127.0.0.1:7890");
            }
            other => panic!("expected the system proxy, got {other:?}"),
        }
    }

    #[test]
    fn bare_host_port_gains_a_scheme() {
        assert_eq!(normalise("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(
            normalise("socks5://127.0.0.1:7891/"),
            "socks5://127.0.0.1:7891"
        );
    }

    #[test]
    fn an_unreachable_proxy_is_reported_instead_of_used() {
        let decision = resolve(
            env_of(&[("HTTPS_PROXY", "http://127.0.0.1:10808")]),
            || None,
            |_, _| false,
        );
        assert_eq!(
            decision,
            Decision::Unreachable("http://127.0.0.1:10808".to_string())
        );
    }

    #[test]
    fn a_reachable_proxy_is_probed_on_its_own_port() {
        let mut seen = None;
        let decision = resolve(
            env_of(&[("HTTPS_PROXY", "http://user:secret@127.0.0.1:7897")]),
            || None,
            |host, port| {
                seen = Some((host.to_string(), port));
                true
            },
        );
        assert!(matches!(decision, Decision::Use(_)));
        assert_eq!(seen, Some(("127.0.0.1".to_string(), 7897)));
    }

    #[test]
    fn every_decision_explains_itself() {
        let proxy = Proxy {
            url: "http://127.0.0.1:10808".to_string(),
            source: ProxySource::Environment,
        };
        assert!(describe(&Decision::Use(proxy)).contains("10808"));
        assert!(describe(&Decision::Disabled).contains("off"));
        assert!(describe(&Decision::Unconfigured).contains("none"));
        assert!(describe(&Decision::Unreachable("x".into())).contains("unreachable"));
    }

    #[test]
    fn kernel_env_exports_the_resolved_proxy() {
        let env = kernel_env(&Decision::Use(Proxy {
            url: "http://127.0.0.1:10808".to_string(),
            source: ProxySource::Explicit,
        }));
        assert!(env.contains(&(
            "HTTPS_PROXY".to_string(),
            "http://127.0.0.1:10808".to_string()
        )));
        assert!(env.contains(&(
            PROXY_ENV.to_string(),
            "http://127.0.0.1:10808".to_string()
        )));
    }

    #[test]
    fn kernel_env_clears_stale_proxy_variables_when_disabled() {
        for decision in [Decision::Disabled, Decision::Unconfigured] {
            let env = kernel_env(&decision);
            for name in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
                assert!(
                    env.contains(&(name.to_string(), String::new())),
                    "{name} must be cleared for {decision:?}"
                );
            }
        }
    }

    #[test]
    fn ipv6_endpoints_are_parsed() {
        let proxy = Proxy {
            url: "http://[::1]:7890".to_string(),
            source: ProxySource::Explicit,
        };
        assert_eq!(proxy.endpoint(), Some(("::1".to_string(), 7890)));
    }

    /// `reg` reports `ProxyEnable` as REG_DWORD (`0x1`) and `ProxyServer` as
    /// REG_SZ. Reading only REG_SZ silently disabled the system-proxy path.
    #[test]
    fn registry_values_are_read_in_both_encodings() {
        let parse = |line: &str, name: &str| -> Option<String> {
            let mut fields = line.split_whitespace();
            if !fields.next()?.eq_ignore_ascii_case(name) {
                return None;
            }
            let kind = fields.next()?;
            let raw = fields.collect::<Vec<_>>().join(" ");
            if raw.is_empty() {
                return None;
            }
            if kind.eq_ignore_ascii_case("REG_DWORD") {
                let value = raw.trim_start_matches("0x");
                return Some(u64::from_str_radix(value, 16).ok()?.to_string());
            }
            Some(raw)
        };
        assert_eq!(parse("    ProxyEnable    REG_DWORD    0x1", "ProxyEnable"), Some("1".to_string()));
        assert_eq!(
            parse("    ProxyServer    REG_SZ    127.0.0.1:10808", "ProxyServer"),
            Some("127.0.0.1:10808".to_string())
        );
        assert_eq!(
            parse("    ProxyOverride   REG_SZ    <local>;localhost", "ProxyOverride"),
            Some("<local>;localhost".to_string())
        );
        assert_eq!(parse("    ProxyEnable    REG_DWORD    0x1", "ProxyServer"), None);
    }

    /// The real machine's `ProxyEnable` is a REG_DWORD, so a fix that only
    /// accepts REG_SZ leaves the system-proxy source permanently dead.
    #[test]
    #[ignore]
    fn system_proxy_reads_the_real_registry() {
        println!("system_proxy() = {:?}", system_proxy());
    }
}
