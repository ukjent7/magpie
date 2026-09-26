use std::{
    env,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use reqwest::{Client, ClientBuilder, Proxy};
use url::Url;

const SYSTEM_PROXY_TTL: Duration = Duration::from_secs(15);

#[derive(Clone, Default)]
struct SystemProxy {
    url: Option<Url>,
    bypass: Vec<String>,
}

static SYSTEM_PROXY: Mutex<Option<(Instant, SystemProxy)>> = Mutex::new(None);

pub(crate) fn builder() -> ClientBuilder {
    Client::builder().no_proxy().proxy(Proxy::custom(proxy_for))
}

pub(crate) fn configure_process_proxy(command: &mut tokio::process::Command, host: &str) {
    const PROXY_VARIABLES: &[&str] = &[
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];

    let configured = crate::settings::load().proxy;
    let configured = configured.trim();
    if configured == "direct" {
        for variable in PROXY_VARIABLES {
            command.env_remove(*variable);
        }
        return;
    }

    let (url, bypass) = if configured.is_empty() {
        if [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ]
        .iter()
        .any(|variable| env::var_os(*variable).is_some_and(|value| !value.is_empty()))
        {
            return;
        }
        let system = system_proxy();
        let Some(url) = system.url else {
            return;
        };
        if bypassed(host, system.bypass.iter().map(String::as_str), false) {
            return;
        }
        (url.to_string(), system.bypass)
    } else {
        let Some(proxy) = parse_proxy(configured) else {
            return;
        };
        (proxy.to_string(), Vec::new())
    };

    let mut no_proxy = vec!["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    no_proxy.extend(bypass.into_iter().filter_map(|entry| {
        let entry = entry.trim().trim_start_matches('*');
        (!entry.is_empty() && entry != "<local>").then(|| entry.to_owned())
    }));
    let no_proxy = no_proxy.join(",");
    for variable in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        command.env(variable, &url);
    }
    for variable in ["ALL_PROXY", "all_proxy"] {
        command.env(variable, &url);
    }
    command.env("NO_PROXY", &no_proxy).env("no_proxy", no_proxy);
}

fn proxy_for(request: &Url) -> Option<Url> {
    let host = request.host_str()?;
    if is_loopback(host) {
        return None;
    }

    let configured = crate::settings::load().proxy;
    match configured.trim() {
        "direct" => return None,
        "" => {}
        configured => return parse_proxy(configured),
    }

    if let Some(proxy) = environment_proxy(request, host) {
        return proxy;
    }

    let system = system_proxy();
    if bypassed(host, system.bypass.iter().map(String::as_str), false) {
        return None;
    }
    system.url
}

fn environment_proxy(request: &Url, host: &str) -> Option<Option<Url>> {
    let bypass = env::var("NO_PROXY")
        .or_else(|_| env::var("no_proxy"))
        .unwrap_or_default();
    if bypassed(host, bypass.split(','), true) {
        return None;
    }

    let variable = match request.scheme() {
        "https" => ["HTTPS_PROXY", "https_proxy"],
        "http" => {
            if env::var_os("REQUEST_METHOD").is_some() {
                ["http_proxy", "http_proxy"]
            } else {
                ["HTTP_PROXY", "http_proxy"]
            }
        }
        _ => return Some(None),
    };
    variable
        .into_iter()
        .chain(["ALL_PROXY", "all_proxy"])
        .filter_map(|name| env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .map(|value| parse_proxy(&value))
}

fn parse_proxy(value: &str) -> Option<Url> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let with_scheme = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let proxy = Url::parse(&with_scheme).ok()?;
    matches!(
        proxy.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    )
    .then_some(proxy)
    .filter(|proxy| proxy.host_str().is_some())
}

fn system_proxy() -> SystemProxy {
    let mut cache = SYSTEM_PROXY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((updated, proxy)) = cache.as_ref()
        && updated.elapsed() < SYSTEM_PROXY_TTL
    {
        return proxy.clone();
    }

    let proxy = platform_system_proxy();
    *cache = Some((Instant::now(), proxy.clone()));
    proxy
}

fn is_loopback(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn bypassed<'a>(
    host: &str,
    rules: impl IntoIterator<Item = &'a str>,
    plain_domains_match_subdomains: bool,
) -> bool {
    let host = host.to_ascii_lowercase();
    rules.into_iter().any(|rule| {
        let rule = rule.trim().to_ascii_lowercase();
        if rule.is_empty() {
            return false;
        }
        if rule == "*" {
            return true;
        }
        if rule == "<local>" {
            return !host.contains('.');
        }

        let rule_host = rule_host(&rule);
        if ip_in_network(&host, rule_host) {
            return true;
        }
        if let Some(suffix) = rule_host.strip_prefix("*.") {
            return host == suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'));
        }
        if let Some(suffix) = rule_host.strip_prefix('.') {
            return host == suffix
                || host
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'));
        }
        host == rule_host
            || (plain_domains_match_subdomains
                && rule_host.contains('.')
                && !rule_host.contains('/')
                && rule_host.parse::<IpAddr>().is_err()
                && host
                    .strip_suffix(rule_host)
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

fn rule_host(rule: &str) -> &str {
    if let Some(bracketed) = rule.strip_prefix('[')
        && let Some((host, _)) = bracketed.split_once(']')
    {
        return host;
    }
    if rule.parse::<IpAddr>().is_ok() || rule.contains('/') {
        return rule;
    }
    if let Some((host, port)) = rule.rsplit_once(':')
        && !host.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return host;
    }
    rule
}

fn ip_in_network(host: &str, rule: &str) -> bool {
    let Some((address, prefix)) = rule.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u32>() else {
        return false;
    };
    match (host.parse::<IpAddr>(), address.parse::<IpAddr>()) {
        (Ok(IpAddr::V4(host)), Ok(IpAddr::V4(network))) if prefix <= 32 => {
            let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
            u32::from(host) & mask == u32::from(network) & mask
        }
        (Ok(IpAddr::V6(host)), Ok(IpAddr::V6(network))) if prefix <= 128 => {
            let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
            u128::from(host) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

#[cfg(target_os = "windows")]
fn platform_system_proxy() -> SystemProxy {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let Some(enabled) = registry_value(KEY, "ProxyEnable") else {
        return SystemProxy::default();
    };
    let enabled = enabled
        .strip_prefix("0x")
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .or_else(|| enabled.parse::<u32>().ok())
        .unwrap_or_default();
    if enabled == 0 {
        return SystemProxy::default();
    }
    let Some(server) = registry_value(KEY, "ProxyServer") else {
        return SystemProxy::default();
    };
    let bypass = registry_value(KEY, "ProxyOverride")
        .unwrap_or_default()
        .split(';')
        .map(str::to_owned)
        .collect();
    windows_proxy(&server, bypass)
}

#[cfg(target_os = "windows")]
fn registry_value(key: &str, name: &str) -> Option<String> {
    let output = crate::proc::command("reg.exe")
        .args(["query", key, "/v", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == name).then(|| fields.nth(1).map(str::to_owned))?
        })
}

#[cfg(target_os = "windows")]
fn windows_proxy(server: &str, bypass: Vec<String>) -> SystemProxy {
    let server = server.trim();
    if server.is_empty() {
        return SystemProxy::default();
    }
    let selected = if server.contains('=') {
        let entries = server
            .split(';')
            .filter_map(|entry| entry.split_once('='))
            .map(|(scheme, value)| (scheme.trim().to_ascii_lowercase(), value.trim()))
            .collect::<std::collections::HashMap<_, _>>();
        [
            ("https", "http://"),
            ("http", "http://"),
            ("socks", "socks5://"),
        ]
        .into_iter()
        .find_map(|(key, prefix)| entries.get(key).map(|value| (*value, prefix)))
    } else {
        Some((server, "http://"))
    };
    let Some((value, prefix)) = selected else {
        return SystemProxy::default();
    };
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("{prefix}{value}")
    };
    SystemProxy {
        url: parse_proxy(&value),
        bypass,
    }
}

#[cfg(target_os = "macos")]
fn platform_system_proxy() -> SystemProxy {
    let Ok(output) = crate::proc::command("scutil").arg("--proxy").output() else {
        return SystemProxy::default();
    };
    if !output.status.success() {
        return SystemProxy::default();
    }
    parse_scutil(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "macos")]
fn parse_scutil(output: &str) -> SystemProxy {
    let mut values = std::collections::HashMap::new();
    let mut bypass = Vec::new();
    let mut in_exceptions = false;
    for line in output.lines().map(str::trim) {
        if in_exceptions {
            if line == "}" {
                in_exceptions = false;
            } else if let Some((_, value)) = line.split_once(" : ") {
                bypass.push(value.to_owned());
            }
            continue;
        }
        let Some((key, value)) = line.split_once(" : ") else {
            continue;
        };
        if key == "ExceptionsList" {
            in_exceptions = true;
        } else {
            values.insert(key.to_owned(), value.to_owned());
        }
    }
    for (key, scheme) in [("HTTPS", "http"), ("HTTP", "http"), ("SOCKS", "socks5")] {
        if values.get(&format!("{key}Enable")).map(String::as_str) != Some("1") {
            continue;
        }
        let Some(host) = values.get(&format!("{key}Proxy")) else {
            continue;
        };
        if host.is_empty() {
            continue;
        }
        let address = values
            .get(&format!("{key}Port"))
            .filter(|port| !port.is_empty())
            .map_or_else(|| host.clone(), |port| join_host_port(host, port));
        return SystemProxy {
            url: parse_proxy(&format!("{scheme}://{address}")),
            bypass,
        };
    }
    SystemProxy::default()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_system_proxy() -> SystemProxy {
    SystemProxy::default()
}

#[cfg(target_os = "macos")]
fn join_host_port(host: &str, port: &str) -> String {
    if host.starts_with('[') || !host.contains(':') {
        format!("{host}:{port}")
    } else {
        format!("[{host}]:{port}")
    }
}
