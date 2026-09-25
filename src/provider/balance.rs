use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use super::Provider;

const MAX_RESPONSE_BYTES: usize = 1 << 20;

enum Format {
    DeepSeek,
    Moonshot(&'static str),
    OpenRouter,
    SiliconFlow(&'static str),
    Custom(String),
}

struct Source {
    url: String,
    format: Format,
}

pub(super) async fn fetch(provider: &Provider) -> Option<Result<String>> {
    let source = source(provider)?;
    let key = if !provider.key.is_empty() {
        provider.key.as_str()
    } else {
        provider
            .keys
            .iter()
            .find(|key| !key.off && !key.key.is_empty())?
            .key
            .as_str()
    };
    Some(fetch_from(provider, key, source).await)
}

async fn fetch_from(provider: &Provider, key: &str, source: Source) -> Result<String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("create provider balance HTTP client")?;
    let mut request = client
        .get(&source.url)
        .bearer_auth(key)
        .header("accept", "application/json");
    for (name, value) in &provider.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| anyhow!("request provider balance: {}", error.without_url()))?;
    let status = response.status();
    let mut bytes = Vec::with_capacity(4096);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("read provider balance: {}", error.without_url()))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_RESPONSE_BYTES,
            "provider balance response exceeds 1 MB"
        );
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        bail!(http_error(status, &bytes));
    }
    parse_balance(&bytes, source.format)
}

fn source(provider: &Provider) -> Option<Source> {
    if !provider.balance_url.is_empty() {
        return Some(Source {
            url: provider.balance_url.clone(),
            format: Format::Custom(provider.balance_path.clone()),
        });
    }

    [&provider.chat, &provider.responses, &provider.anthropic]
        .into_iter()
        .filter_map(|base| Url::parse(base).ok())
        .find_map(|url| {
            let host = url.host_str()?;
            let (url, format) = match host {
                "api.deepseek.com" => (
                    "https://api.deepseek.com/user/balance".to_owned(),
                    Format::DeepSeek,
                ),
                "api.moonshot.cn" => (
                    "https://api.moonshot.cn/v1/users/me/balance".to_owned(),
                    Format::Moonshot("¥"),
                ),
                "api.moonshot.ai" => (
                    "https://api.moonshot.ai/v1/users/me/balance".to_owned(),
                    Format::Moonshot("$"),
                ),
                "openrouter.ai" => (
                    "https://openrouter.ai/api/v1/credits".to_owned(),
                    Format::OpenRouter,
                ),
                "api.siliconflow.cn" => (
                    "https://api.siliconflow.cn/v1/user/info".to_owned(),
                    Format::SiliconFlow("¥"),
                ),
                "api.siliconflow.com" => (
                    "https://api.siliconflow.com/v1/user/info".to_owned(),
                    Format::SiliconFlow("$"),
                ),
                _ => return None,
            };
            Some(Source { url, format })
        })
}

fn parse_balance(bytes: &[u8], format: Format) -> Result<String> {
    let value = serde_json::from_slice::<Value>(bytes).context("balance reply is not JSON")?;
    match format {
        Format::DeepSeek => {
            let mut balances = Vec::new();
            if let Some(items) = value.get("balance_infos").and_then(Value::as_array) {
                for item in items {
                    let amount = item.get("total_balance").and_then(number);
                    if let Some(amount) = amount {
                        let currency = item
                            .get("currency")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let sign = currency_sign(currency);
                        balances.push(money(&sign, amount));
                    }
                }
            }
            ensure!(!balances.is_empty(), "no balance in the reply");
            Ok(balances.join(" · "))
        }
        Format::Moonshot(sign) => {
            let amount = value
                .pointer("/data/available_balance")
                .and_then(number)
                .context("no balance in the reply")?;
            Ok(money(sign, amount))
        }
        Format::OpenRouter => {
            let credits = value
                .pointer("/data/total_credits")
                .and_then(number)
                .context("no credits in the reply")?;
            let used = value
                .pointer("/data/total_usage")
                .and_then(number)
                .unwrap_or_default();
            Ok(money("$", credits - used))
        }
        Format::SiliconFlow(sign) => {
            let amount = value
                .pointer("/data/totalBalance")
                .and_then(number)
                .context("no balance in the reply")?;
            Ok(money(sign, amount))
        }
        Format::Custom(path) => custom_balance(&value, &path),
    }
}

fn custom_balance(value: &Value, path: &str) -> Result<String> {
    let mut path = path.trim();
    let mut sign = "";
    for candidate in ["$", "¥", "€", "£"] {
        if let Some(rest) = path.strip_prefix(candidate) {
            sign = candidate;
            path = rest.trim();
            break;
        }
    }
    let (path, divisor) = match path.split_once('/') {
        Some((path, divisor)) => {
            let divisor_text = divisor.trim();
            let divisor = divisor_text.parse::<f64>().with_context(|| {
                format!("the balance path divides by {divisor_text:?}, not a number")
            })?;
            ensure!(divisor != 0.0, "the balance path divisor cannot be zero");
            (path.trim(), divisor)
        }
        None => (path, 1.0),
    };
    ensure!(
        !path.is_empty(),
        "no balance path: name where the amount is, e.g. data.balance"
    );

    let mut amount = value;
    for segment in path.split('.') {
        amount = match amount {
            Value::Object(object) => object.get(segment),
            Value::Array(array) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| array.get(index)),
            _ => None,
        }
        .with_context(|| format!("nothing at {path:?} in the reply"))?;
    }
    if let Some(amount) = number(amount) {
        return Ok(money(sign, amount / divisor));
    }
    if let Some(amount) = amount.as_str().filter(|amount| !amount.is_empty()) {
        return Ok(format!("{sign}{amount}"));
    }
    bail!("{path:?} in the reply is not an amount")
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn money(sign: &str, amount: f64) -> String {
    format!("{sign}{amount:.2}")
}

fn currency_sign(currency: &str) -> String {
    match currency.to_ascii_uppercase().as_str() {
        "CNY" | "RMB" => "¥".to_owned(),
        "USD" => "$".to_owned(),
        "" => String::new(),
        other => format!("{other} "),
    }
}

fn http_error(status: StatusCode, bytes: &[u8]) -> String {
    let body = String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !body.starts_with('{') {
        return status.to_string();
    }
    let message = body.chars().take(200).collect::<String>();
    if body.chars().count() > 200 {
        format!("{status}: {message}…")
    } else {
        format!("{status}: {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vendor_balances() {
        assert_eq!(
            parse_balance(
                br#"{"balance_infos":[{"currency":"CNY","total_balance":"110.00"}]}"#,
                Format::DeepSeek
            )
            .unwrap(),
            "¥110.00"
        );
        assert_eq!(
            parse_balance(
                br#"{"data":{"total_credits":20,"total_usage":3.5}}"#,
                Format::OpenRouter
            )
            .unwrap(),
            "$16.50"
        );
    }

    #[test]
    fn custom_paths_support_arrays_currencies_and_divisors() {
        let value = serde_json::json!({"balances": [{"amount": "2500000"}]});
        assert_eq!(
            custom_balance(&value, "$balances.0.amount / 500000").unwrap(),
            "$5.00"
        );
    }
}
