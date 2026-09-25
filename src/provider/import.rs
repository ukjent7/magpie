use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{self, IsTerminal, Write as _},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{StatusCode, redirect::Policy};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use super::{
    MAX_ICON_BYTES, PRESETS, PresetRegion, Provider, find_preset, load, mask, prepare_provider,
    store,
};

pub(crate) async fn command(args: &[String]) -> Result<()> {
    let mut confirm_automatically = false;
    let mut link = None;
    for argument in args {
        match argument.as_str() {
            "-y" | "--yes" => confirm_automatically = true,
            value if value.starts_with('-') => {
                bail!("unknown flag {value} (magpie import [-y] <link>)")
            }
            _ if link.is_none() => link = Some(argument.as_str()),
            _ => bail!("usage: magpie import [-y] <link>"),
        }
    }
    let link = link.context("usage: magpie import [-y] <link>")?;
    let mut provider = parse_import(link)?;
    let existing = load()?
        .providers
        .into_iter()
        .find(|saved| saved.id == provider.id);
    if let Some(existing) = existing {
        println!("! replaces your {}", existing.name);
        if provider.key.is_empty() {
            provider.key.clone_from(&existing.key);
            provider.key_name.clone_from(&existing.key_name);
            provider.keys.clone_from(&existing.keys);
            provider.key_protocol.clone_from(&existing.key_protocol);
        }
    }
    show_preview(&provider);

    if !confirm_automatically {
        ensure!(
            io::stdin().is_terminal(),
            "not a terminal: add -y to import without asking"
        );
        print!("Add it? [y/N] ");
        io::stdout().flush().context("write import prompt")?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("read import confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("nothing added");
        }
    }

    if provider.key.is_empty() && !provider.is_local() {
        ensure!(
            io::stdin().is_terminal(),
            "the link has no API key; add key= or run magpie import without -y to enter one"
        );
        provider.key = rpassword::prompt_password("API key: ").context("read API key")?;
        ensure!(!provider.key.is_empty(), "the API key is empty");
    }

    if !provider.icon_url.is_empty() {
        match fetch_icon(&provider.icon_url).await {
            Ok(icon) => provider.icon = icon,
            Err(error) => eprintln!("magpie: couldn't fetch provider icon: {error:#}"),
        }
    }
    provider.icon_url.clear();
    let provider = prepare_provider(provider)?;
    let id = provider.id.clone();
    save(provider)?;
    if let Err(error) = super::models_command(&id, &[]).await {
        eprintln!("magpie: couldn't fetch models yet: {error:#}");
    }
    Ok(())
}

fn parse_import(link: &str) -> Result<Provider> {
    let url = Url::parse(link.trim()).map_err(|_| anyhow::anyhow!("not a magpie:// link"))?;
    ensure!(url.scheme() == "magpie", "not a magpie:// link");
    let action = url
        .host_str()
        .unwrap_or_else(|| url.path())
        .trim_matches('/');
    ensure!(
        action == "import",
        "magpie://{action} is not something magpie knows; links start magpie://import?"
    );

    let mut parameters = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        parameters
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    let parameter = |name: &str| parameters.get(name).map_or("", |value| value.trim());

    let preset_id = parameter("preset").to_ascii_lowercase();
    let preset = if preset_id.is_empty() {
        None
    } else {
        Some(find_preset(&preset_id).with_context(|| format!("no preset {preset_id:?}"))?)
    };
    let mut provider = if let Some(preset) = preset {
        let mut provider = preset.provider();
        if !parameter("region").is_empty() {
            let region = preset
                .regions
                .iter()
                .find(|region| region.id == parameter("region"))
                .with_context(|| {
                    format!("{} has no region {:?}", preset.name, parameter("region"))
                })?;
            apply_region(&mut provider, region);
        }
        if !parameter("name").is_empty() {
            provider.name = parameter("name").to_owned();
        }
        provider
    } else {
        let catalog = parameter("catalog").to_ascii_lowercase();
        let catalog = if catalog == super::slug(&catalog) {
            catalog
        } else {
            String::new()
        };
        let mut provider = Provider {
            name: parameter("name").to_owned(),
            catalog,
            ..Provider::default()
        };
        ensure!(
            !provider.name.is_empty(),
            "the link names no provider: it needs preset= or name="
        );
        provider.chat = endpoint("chat", parameter("chat"))?;
        provider.responses = endpoint("responses", parameter("responses"))?;
        provider.anthropic = endpoint("anthropic", parameter("anthropic"))?;
        ensure!(
            !provider.chat.is_empty()
                || !provider.responses.is_empty()
                || !provider.anthropic.is_empty(),
            "the link gives no base URL: it needs chat=, responses= or anthropic="
        );
        if !provider.catalog.is_empty() {
            provider.icon = PRESETS
                .iter()
                .find(|preset| preset.catalog == provider.catalog)
                .map_or(String::new(), |preset| preset.icon.to_owned());
        }
        provider.website = page(parameter("website"));
        provider.keys_url = page(parameter("keys"));
        provider
    };

    if !parameter("icon").is_empty() {
        provider.icon_url = icon_url(parameter("icon"))?.to_string();
    }
    if !parameter("id").is_empty() {
        provider.id = super::slug(parameter("id"));
    } else if preset.is_none() {
        provider.id = super::slug(&provider.name);
    }
    ensure!(
        !provider.id.is_empty(),
        "the link's name has no letters or digits to make an id from"
    );
    ensure!(
        provider.id != "magpie" && provider.id != "group",
        "the link uses a reserved provider id"
    );
    provider.name = provider.name.chars().take(80).collect();
    provider.key = parameter("key").to_owned();
    ensure!(
        provider.key.len() <= 4096 && !provider.key.chars().any(char::is_control),
        "the link's key is not a key"
    );
    provider.models = parameter("models")
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .take(200)
        .map(str::to_owned)
        .collect();
    Ok(provider)
}

fn apply_region(provider: &mut Provider, region: &PresetRegion) {
    provider.chat = region.chat.to_owned();
    provider.responses = region.responses.to_owned();
    provider.anthropic = region.anthropic.to_owned();
}

fn endpoint(name: &str, raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let url = Url::parse(raw).with_context(|| format!("the link's {name}= is not a base URL"))?;
    ensure!(
        url.host().is_some()
            && !has_userinfo(&url)
            && url.query().is_none()
            && url.fragment().is_none(),
        "the link's {name}= is not a base URL"
    );
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http" && local_host(url.host_str().unwrap_or_default())),
        "the link's {name}= must use https unless it points to the local network"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn page(raw: &str) -> String {
    let Ok(url) = Url::parse(raw) else {
        return String::new();
    };
    if url.scheme() == "https" && url.host().is_some() && !has_userinfo(&url) {
        url.to_string()
    } else {
        String::new()
    }
}

fn icon_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).context("the link's icon= is not a picture URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host().is_some()
            && !has_userinfo(&url)
            && url.fragment().is_none(),
        "the link's icon= must be a public https picture URL"
    );
    ensure!(
        !local_host(url.host_str().unwrap_or_default()),
        "the link's icon= must be on a public host"
    );
    Ok(url)
}

fn has_userinfo(url: &Url) -> bool {
    url.as_str()
        .split_once("://")
        .and_then(|(_, remainder)| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn local_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower == "0.0.0.0" || lower.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || ip.is_unique_local() || ip.is_unspecified(),
        Err(_) => false,
    }
}

async fn fetch_icon(raw_url: &str) -> Result<String> {
    let url = icon_url(raw_url)?;
    let port = url
        .port_or_known_default()
        .context("the icon URL has no port")?;
    let (domain, addresses) = match url.host().context("the icon URL has no host")? {
        Host::Domain(domain) => {
            let addresses = tokio::time::timeout(
                Duration::from_secs(15),
                tokio::net::lookup_host((domain, port)),
            )
            .await
            .context("timed out resolving icon host")?
            .with_context(|| format!("resolve icon host {domain}"))?
            .collect::<Vec<_>>();
            ensure!(!addresses.is_empty(), "could not resolve the icon host");
            ensure!(
                addresses.iter().all(|address| is_public_ip(address.ip())),
                "the icon host resolves to an address that is not public"
            );
            (Some(domain.to_owned()), addresses)
        }
        Host::Ipv4(ip) => {
            ensure!(is_public_ipv4(ip), "the icon host is not public");
            (None, vec![SocketAddr::new(IpAddr::V4(ip), port)])
        }
        Host::Ipv6(ip) => {
            ensure!(is_public_ipv6(ip), "the icon host is not public");
            (None, vec![SocketAddr::new(IpAddr::V6(ip), port)])
        }
    };
    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(15));
    if let Some(domain) = domain {
        builder = builder.resolve_to_addrs(&domain, &addresses);
    }
    let client = builder.build().context("build icon HTTP client")?;
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "image/*")
        .send()
        .await
        .context("could not fetch the provider icon")?;
    ensure!(
        response.status() == StatusCode::OK,
        "the icon URL answered {}",
        response.status()
    );
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_ICON_BYTES as u64,
            "the picture is over 1 MB; pick a smaller one"
        );
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read provider icon")? {
        ensure!(
            data.len().saturating_add(chunk.len()) <= MAX_ICON_BYTES,
            "the picture is over 1 MB; pick a smaller one"
        );
        data.extend_from_slice(&chunk);
    }
    store_icon(&data)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (first == 169 && second == 254)
        || (first == 100 && second & 0xc0 == 64)
        || (first == 192 && second == 0 && matches!(third, 0 | 2))
        || (first == 198 && matches!(second, 18 | 19))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00 == 0xfc00)
        || (segments[0] & 0xffc0 == 0xfe80)
        || segments[0] == 0x2002
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn store_icon(data: &[u8]) -> Result<String> {
    ensure!(!data.is_empty(), "the picture is empty");
    ensure!(
        data.len() <= MAX_ICON_BYTES,
        "the picture is over 1 MB; pick a smaller one"
    );
    let extension = icon_extension(data)
        .context("not a picture magpie can show: use PNG, JPEG, GIF, WebP, ICO or SVG")?;
    let digest = Sha256::digest(data);
    let mut name = String::with_capacity(20);
    for byte in &digest[..8] {
        let _ = write!(name, "{byte:02x}");
    }
    write!(name, ".{extension}").expect("writing to a String cannot fail");
    let path = crate::settings::providers_path()
        .parent()
        .context("provider path has no parent directory")?
        .join("icons");
    fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    crate::config::atomic_write_for_settings(&path.join(&name), data)
        .with_context(|| format!("write provider icon {}", path.join(&name).display()))?;
    Ok(format!("file:{name}"))
}

fn icon_extension(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else if data.starts_with(&[0, 0, 1, 0]) {
        Some("ico")
    } else {
        let header = data[..data.len().min(1024)].to_ascii_lowercase();
        header
            .windows(4)
            .any(|window| window == b"<svg")
            .then_some("svg")
    }
}

fn show_preview(provider: &Provider) {
    println!("{} ({})", provider.name, provider.id);
    for (name, endpoint) in [
        ("chat", provider.chat.as_str()),
        ("responses", provider.responses.as_str()),
        ("anthropic", provider.anthropic.as_str()),
    ] {
        if !endpoint.is_empty() {
            println!("  {name:10} {endpoint}");
        }
    }
    let key = match provider.key.as_str() {
        key if !key.is_empty() => mask(key),
        _ if provider.is_local() => "none needed".to_owned(),
        _ => "not set".to_owned(),
    };
    println!("  {:10} {key}", "key");
    if !provider.models.is_empty() {
        println!("  {:10} {}", "models", provider.models.join(", "));
    }
    if !provider.catalog.is_empty() {
        println!("  {:10} {}", "catalog", provider.catalog);
    }
    if !provider.website.is_empty() {
        println!("  {:10} {}", "website", provider.website);
    }
    if !provider.keys_url.is_empty() {
        println!("  {:10} {}", "keys", provider.keys_url);
    }
    if !provider.icon_url.is_empty() {
        println!(
            "  {:10} custom picture, downloaded after confirmation",
            "icon"
        );
    }
}

fn save(provider: Provider) -> Result<()> {
    let mut file = load()?;
    if let Some(existing) = file
        .providers
        .iter_mut()
        .find(|existing| existing.id == provider.id)
    {
        *existing = provider.clone();
    } else {
        file.providers.push(provider.clone());
    }
    store(file)?;
    println!("✓ added {} ({})", provider.name, provider.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preset_and_model_selection() {
        let provider = parse_import(
            "magpie://import?preset=deepseek&key=sk-abc123&models=deepseek-v4-pro,%20deepseek-v4-flash",
        )
        .expect("parse preset link");
        assert_eq!(provider.id, "deepseek");
        assert_eq!(provider.preset, "deepseek");
        assert_eq!(provider.key, "sk-abc123");
        assert_eq!(provider.chat, "https://api.deepseek.com/v1");
        assert_eq!(
            provider.models,
            vec!["deepseek-v4-pro".to_owned(), "deepseek-v4-flash".to_owned()]
        );
    }

    #[test]
    fn parses_custom_links_and_ignores_unsafe_vendor_pages() {
        let provider = parse_import(
            "magpie://import?name=My%20Relay&chat=https%3A%2F%2Frelay.example%2Fv1%2F&anthropic=https%3A%2F%2Frelay.example&key=sk-relay&website=https%3A%2F%2Frelay.example&keys=javascript%3Aalert%281%29&catalog=openai",
        )
        .expect("parse custom link");
        assert_eq!(provider.id, "my-relay");
        assert_eq!(provider.name, "My Relay");
        assert_eq!(provider.chat, "https://relay.example/v1");
        assert_eq!(provider.anthropic, "https://relay.example");
        assert_eq!(provider.website, "https://relay.example");
        assert!(provider.keys_url.is_empty());
        assert_eq!(provider.catalog, "openai");
        assert_eq!(provider.icon, "openai");
    }

    #[test]
    fn parses_supported_scheme_forms_and_regions() {
        for link in [
            "magpie:import?preset=deepseek",
            "magpie:///import?preset=deepseek",
            "MAGPIE://import/?preset=deepseek",
        ] {
            assert!(parse_import(link).is_ok(), "{link}");
        }
        let provider = parse_import("magpie://import?preset=yylx&region=global")
            .expect("parse regional preset");
        assert_eq!(provider.chat, "https://global.yylx.io/v1");
        assert_eq!(provider.anthropic, "https://global.yylx.io");
        assert!(parse_import("magpie://import?preset=yylx&region=unknown").is_err());
    }

    #[test]
    fn rejects_malformed_or_unsafe_import_links() {
        for link in [
            "https://usemagpie.ai/import?preset=deepseek",
            "magpie://delete?id=deepseek",
            "magpie://import",
            "magpie://import?preset=unknown",
            "magpie://import?name=Relay",
            "magpie://import?name=Relay&chat=http://relay.example/v1",
            "magpie://import?name=Relay&chat=https://user:pw@relay.example/v1",
            "magpie://import?name=Relay&chat=file:///etc/passwd",
            "magpie://import?name=magpie&chat=https://relay.example/v1",
            "magpie://import?name=%E2%80%94&chat=https://relay.example/v1",
            "magpie://import?preset=deepseek&key=sk%0Aevil",
        ] {
            assert!(parse_import(link).is_err(), "accepted {link}");
        }
        for url in [
            "http://relay.example/logo.svg",
            "file:///etc/passwd",
            "https://user:pw@relay.example/logo.svg",
            "https://relay.example/logo.svg#fragment",
            "https://localhost/logo.svg",
            "https://127.0.0.1/logo.svg",
            "not a url",
        ] {
            assert!(icon_url(url).is_err(), "accepted icon {url}");
        }
    }

    #[test]
    fn accepts_local_http_but_restricts_public_icon_addresses() {
        for host in [
            "localhost:11434",
            "127.0.0.1:8080",
            "192.168.1.20:8000",
            "box.local",
        ] {
            let link = format!("magpie://import?name=Local&chat=http%3A%2F%2F{host}%2Fv1");
            assert!(parse_import(&link).is_ok(), "{host}");
        }
        for address in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            assert!(!is_public_ip(address), "accepted {address}");
        }
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn icon_urls_are_never_serialized_into_provider_settings() {
        let provider = Provider {
            icon_url: "https://images.example/logo.svg".to_owned(),
            ..Provider::default()
        };
        let value = serde_json::to_value(provider).expect("serialize provider");
        assert!(value.get("iconUrl").is_none());
    }
}
