use std::{
    cmp::Ordering,
    collections::HashMap,
    env, fs,
    net::IpAddr,
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const SITE: &str = "https://usemagpie.ai";
const VERSION: &str = crate::VERSION;
const MAX_FEED_BYTES: usize = 1 << 20;
const MAX_BINARY_BYTES: u64 = 512 << 20;

#[derive(Debug, Deserialize)]
struct Release {
    version: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    assets: HashMap<String, Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    sha256: String,
}

pub async fn command(args: &[String]) -> Result<()> {
    let check_only = match args {
        [] => false,
        [arg] if arg == "check" => true,
        _ => bail!("usage: magpie update [check]"),
    };

    let release = latest().await?;
    if !is_newer(&release.version, VERSION) {
        if is_released(VERSION) {
            println!("✓ magpie {VERSION} is the latest");
        } else {
            println!(
                "magpie {VERSION} was built from source; the latest release is {}",
                release.version
            );
        }
        return Ok(());
    }

    let release_url = validate_url(&release.url, false).with_context(|| {
        format!(
            "update feed has an invalid release URL for {}",
            release.version
        )
    })?;
    println!(
        "magpie {} is out (you have {VERSION}) · {release_url}",
        release.version
    );
    if check_only {
        return Ok(());
    }
    ensure!(
        is_released(VERSION),
        "this magpie was built from source; update it the way you built it, or get the release from {SITE}"
    );

    let asset_name = binary_asset_name()?;
    let asset = release
        .assets
        .get(&asset_name)
        .with_context(|| format!("release {} has no {asset_name}", release.version))?;
    let executable = env::current_exe()
        .context("find the running magpie executable")?
        .canonicalize()
        .context("resolve the running magpie executable")?;
    let staged = staged_path(&executable)?;

    println!("  downloading {asset_name} …");
    if let Err(error) = download(asset, &staged).await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(error.context("download magpie update"));
    }
    if let Err(error) = set_executable_permissions(&staged) {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(error);
    }
    if let Err(error) = install(&staged, &executable) {
        let _ = fs::remove_file(&staged);
        return Err(error).with_context(|| {
            format!(
                "could not replace {} · download the new version from {release_url}",
                executable.display()
            )
        });
    }
    println!("✓ updated to {}", release.version);
    Ok(())
}

async fn latest() -> Result<Release> {
    let feed = env::var("MAGPIE_UPDATE_FEED").unwrap_or_else(|_| format!("{SITE}/api/latest"));
    let feed = validate_url(&feed, true).context("invalid update feed URL")?;
    let client = client()?;
    let mut response = client
        .get(feed)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("fetch update feed")?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "update feed: {}",
        response.status()
    );

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_FEED_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.context("read update feed")? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_FEED_BYTES,
            "update feed exceeds 1 MiB"
        );
        body.extend_from_slice(&chunk);
    }

    let release: Release = serde_json::from_slice(&body).context("decode update feed")?;
    ensure!(
        parse_version(&release.version).is_some(),
        "update feed has no valid version"
    );
    Ok(release)
}

fn client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("magpie/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10 * 60))
        .build()
        .context("create update HTTP client")
}

fn validate_url(value: &str, allow_local_http: bool) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("invalid URL {value:?}"))?;
    let local = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host.ends_with(".localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    ensure!(
        url.scheme() == "https" || (allow_local_http && local && url.scheme() == "http"),
        "URL must use HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "URL must not contain user information or a fragment"
    );
    Ok(url)
}

fn binary_asset_name() -> Result<String> {
    let operating_system = match env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        "linux" => "linux",
        os => bail!("self-updating is not supported on {os}"),
    };
    let architecture = match env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => bail!(
            "self-updating is not supported on {}",
            architecture_name(arch)
        ),
    };
    let extension = if operating_system == "windows" {
        ".exe"
    } else {
        ""
    };
    Ok(format!(
        "magpie-cli-{operating_system}-{architecture}{extension}"
    ))
}

fn architecture_name(architecture: &str) -> &'static str {
    match architecture {
        "x86" => "32-bit x86",
        "arm" => "32-bit ARM",
        _ => "this architecture",
    }
}

fn staged_path(executable: &Path) -> Result<PathBuf> {
    let directory = executable
        .parent()
        .context("running executable has no parent directory")?;
    let file_name = executable
        .file_name()
        .context("running executable has no file name")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(directory.join(format!(
        ".{file_name}.magpie-update-{}-{nonce}.new",
        process::id()
    )))
}

async fn download(asset: &Asset, path: &Path) -> Result<()> {
    ensure!(
        asset.sha256.len() == 64 && asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "release does not list a valid SHA-256 checksum"
    );
    let url = validate_url(&asset.url, false).context("update asset URL must use HTTPS")?;
    let client = client()?;
    let mut last_error = None;

    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2_u64 << (attempt - 1))).await;
        }
        match download_attempt(&client, &url, asset, path).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_file(path).await;
                last_error = Some(error);
            }
        }
    }

    Err(last_error.expect("three download attempts always run"))
}

async fn download_attempt(client: &Client, url: &Url, asset: &Asset, path: &Path) -> Result<()> {
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .context("request update asset")?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "download update: {}",
        response.status()
    );
    if let Some(length) = response.content_length() {
        ensure!(length <= MAX_BINARY_BYTES, "update binary exceeds 512 MiB");
        ensure!(
            asset.size == 0 || length == asset.size,
            "update binary size differs from the release feed"
        );
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .await
        .with_context(|| format!("create staged update {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = response.chunk().await.context("read update binary")? {
        size = size
            .checked_add(chunk.len() as u64)
            .context("update binary size overflow")?;
        ensure!(size <= MAX_BINARY_BYTES, "update binary exceeds 512 MiB");
        file.write_all(&chunk)
            .await
            .context("write staged update")?;
        hasher.update(&chunk);
    }
    file.flush().await.context("flush staged update")?;
    file.sync_all().await.context("sync staged update")?;
    drop(file);

    ensure!(
        asset.size == 0 || size == asset.size,
        "update binary size differs from the release feed"
    );
    let digest = format!("{:x}", hasher.finalize());
    ensure!(
        digest.eq_ignore_ascii_case(&asset.sha256),
        "update binary does not match its SHA-256 checksum"
    );
    Ok(())
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .context("make downloaded magpie executable")
}

#[cfg(windows)]
fn set_executable_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn install(staged: &Path, executable: &Path) -> Result<()> {
    fs::rename(staged, executable).context("replace running executable")
}

#[cfg(windows)]
fn install(staged: &Path, executable: &Path) -> Result<()> {
    let old = append_suffix(executable, ".old");
    if old.exists() {
        fs::remove_file(&old).context("remove previous staged executable")?;
    }
    fs::rename(executable, &old).context("move running executable aside")?;
    if let Err(error) = fs::rename(staged, executable) {
        let _ = fs::rename(&old, executable);
        return Err(error).context("install downloaded executable");
    }
    Ok(())
}

#[cfg(windows)]
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedVersion {
    numbers: [u64; 3],
    prerelease: String,
}

fn parse_version(value: &str) -> Option<ParsedVersion> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let (numbers, prerelease) = value.split_once('-').unwrap_or((value, ""));
    let numbers = numbers
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let numbers: [u64; 3] = numbers.try_into().ok()?;
    Some(ParsedVersion {
        numbers,
        prerelease: prerelease.to_owned(),
    })
}

fn compare_versions(left: &ParsedVersion, right: &ParsedVersion) -> Ordering {
    left.numbers.cmp(&right.numbers).then_with(|| {
        match (left.prerelease.is_empty(), right.prerelease.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => left.prerelease.cmp(&right.prerelease),
        }
    })
}

fn is_newer(left: &str, right: &str) -> bool {
    parse_version(left)
        .zip(parse_version(right))
        .is_some_and(|(left, right)| compare_versions(&left, &right) == Ordering::Greater)
}

fn is_released(value: &str) -> bool {
    let Some(version) = parse_version(value) else {
        return false;
    };
    let prerelease = version.prerelease.as_str();
    if prerelease.contains("dirty") {
        return false;
    }
    let Some((commits, hash)) = prerelease.split_once("-g") else {
        return true;
    };
    !commits.is_empty()
        && commits.bytes().all(|byte| byte.is_ascii_digit())
        && !hash.is_empty()
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}
