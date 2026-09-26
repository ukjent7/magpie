use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::StatusCode;
use serde_json::Value;
use url::Url;

use super::Provider;

const MAX_RESPONSE_BYTES: usize = 1 << 20;

enum Format {
    DeepSeek,
    Moonshot(&'static str),
    OpenRouter,
    SiliconFlow(&'static str),
    AiHubMix,
    AiHubMixAccount,
    Custom(String),
}

struct Source {
    url: String,
    format: Format,
    // token, when set, is sent as the whole Authorization header in place
    // of the key's: the balance is the account's, not a key's
    token: String,
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
    let client = crate::netproxy::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("create provider balance HTTP client")?;
    let mut request = client.get(&source.url).header("accept", "application/json");
    if source.token.is_empty() {
        request = request.bearer_auth(key);
    } else {
        request = request.header("authorization", source.token.as_str());
    }
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
            token: String::new(),
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
                "aihubmix.com" => {
                    if !provider.balance_token.is_empty() {
                        (
                            "https://aihubmix.com/api/user/self".to_owned(),
                            Format::AiHubMixAccount,
                        )
                    } else {
                        (
                            "https://aihubmix.com/dashboard/billing/remain".to_owned(),
                            Format::AiHubMix,
                        )
                    }
                }
                _ => return None,
            };
            // the account's balance is the same whichever key asks
            let token = if matches!(format, Format::AiHubMixAccount) {
                provider.balance_token.clone()
            } else {
                String::new()
            };
            Some(Source { url, format, token })
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
        // readAiHubMix: {"total_usage":12.5}, what is left on the key in
        // dollars, despite the name. A key without a limit answers -1 of
        // AiHubMix's units ($1 is 500000 of them): it has no balance of its
        // own, and the account's is told only to the account's access token.
        Format::AiHubMix => {
            let amount = value
                .get("total_usage")
                .and_then(number)
                .context("no balance in the reply")?;
            ensure!(
                amount >= 0.0,
                "this key has no limit, and AiHubMix tells a key only what is left on it: \
                 give magpie the account's access token (balanceToken=) to see the account's \
                 balance, or give the key a limit in AiHubMix's console"
            );
            Ok(money("$", amount))
        }
        // readAiHubMixAccount: {"success":true,"data":{"quota":2500000}},
        // the account's balance in AiHubMix's units, $1 to 500000 of them.
        Format::AiHubMixAccount => {
            ensure!(
                value.get("success").and_then(Value::as_bool) == Some(true),
                "AiHubMix didn't take the access token"
            );
            let amount = value
                .pointer("/data/quota")
                .and_then(number)
                .context("no balance in the reply")?;
            Ok(money("$", amount / 500000.0))
        }
        Format::Custom(path) => custom_balance(&value, &path),
    }
}

// read_balance_path is the balance path's answer: where in the reply the
// amount is, or sums of those, with + - * / and brackets, for a relay
// counting in its own units ("data.total_available / 500000") or telling
// what was used of a plan ("(1 - credits.monthlyCredits / 70) %"). A "$" or
// "¥" before it is put in front of the amount; a "%" after it shows it as a
// percent of 1 (0.25 is 25%). A path alone that is not a number is shown as
// it is. Several amounts, each with a label if wanted, go apart by ";" and
// are shown together: "5h: windowLimits.fiveHour.used / cap %; $credits.left"
// is "5h 0% · $70.00".
fn custom_balance(value: &Value, path: &str) -> Result<String> {
    let path = path.trim();
    if !path.contains(';') && !path.contains(':') {
        return read_balance_one(value, path);
    }
    let mut out = Vec::new();
    for part in path.split(';') {
        if part.trim().is_empty() {
            continue;
        }
        let (label, expr) = match part.split_once(':') {
            Some((label, expr)) => (label.trim(), expr),
            None => ("", part),
        };
        let mut amount =
            read_balance_one(value, expr).map_err(|error| anyhow!("{label}: {error}"))?;
        if !label.is_empty() {
            amount = format!("{label} {amount}");
        }
        out.push(amount);
    }
    ensure!(
        !out.is_empty(),
        "no balance path: where in the reply the amount is, e.g. data.balance"
    );
    Ok(out.join(" · "))
}

// read_balance_one is one amount of a balance path.
fn read_balance_one(value: &Value, path: &str) -> Result<String> {
    let mut path = path.trim();
    let mut sign = "";
    for candidate in ["$", "¥", "€", "£"] {
        if let Some(rest) = path.strip_prefix(candidate) {
            sign = candidate;
            path = rest.trim();
            break;
        }
    }
    let mut percent = false;
    if let Some(rest) = path.strip_suffix('%') {
        percent = true;
        path = rest.trim();
    }
    ensure!(
        !path.is_empty(),
        "no balance path: where in the reply the amount is, e.g. data.balance"
    );

    let mut expression = BalanceExpression::new(path, value);
    if expression.lone() {
        // a path alone: its value, a number or not
        let found = expression.at(path).map_err(|error| anyhow!("{error}"))?;
        if let Some(amount) = number(found) {
            return Ok(balance_amount(sign, amount, percent));
        }
        if !percent && let Some(amount) = found.as_str().filter(|amount| !amount.is_empty()) {
            return Ok(format!("{sign}{amount}"));
        }
        bail!("{path:?} in the reply is not an amount");
    }
    let amount = expression.sum().map_err(|error| anyhow!("{error}"))?;
    expression.space();
    ensure!(
        expression.index >= expression.source.len(),
        "the balance path has {} it can't read",
        expression.source[expression.index..]
            .iter()
            .collect::<String>()
    );
    ensure!(amount.is_finite(), "the balance path divides by nothing");
    Ok(balance_amount(sign, amount, percent))
}

fn balance_amount(sign: &str, amount: f64, percent: bool) -> String {
    if percent {
        let value = format!("{:.1}", amount * 100.0);
        let value = value.strip_suffix(".0").unwrap_or(&value);
        return format!("{sign}{value}%");
    }
    money(sign, amount)
}

// BalanceExpression reads a balance path's sum: paths into the reply,
// numbers, + - * / and brackets.
struct BalanceExpression<'a> {
    source: Vec<char>,
    index: usize,
    reply: &'a Value,
}

impl<'a> BalanceExpression<'a> {
    fn new(source: &str, reply: &'a Value) -> Self {
        Self {
            source: source.chars().collect(),
            index: 0,
            reply,
        }
    }

    fn lone(&self) -> bool {
        !self.source.is_empty()
            && !self
                .source
                .iter()
                .any(|c| matches!(c, '+' | '-' | '*' | '/' | '(' | ')' | ' '))
            && !self.source[0].is_ascii_digit()
    }

    fn space(&mut self) {
        while self.index < self.source.len() && self.source[self.index] == ' ' {
            self.index += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.source.get(self.index).copied()
    }

    fn sum(&mut self) -> std::result::Result<f64, String> {
        let mut value = self.product()?;
        loop {
            self.space();
            let operator = match self.peek() {
                Some(operator @ ('+' | '-')) => operator,
                _ => break,
            };
            self.index += 1;
            let operand = self.product()?;
            if operator == '+' {
                value += operand;
            } else {
                value -= operand;
            }
        }
        Ok(value)
    }

    fn product(&mut self) -> std::result::Result<f64, String> {
        let mut value = self.unary()?;
        loop {
            self.space();
            let operator = match self.peek() {
                Some(operator @ ('*' | '/')) => operator,
                _ => break,
            };
            self.index += 1;
            let operand = self.unary()?;
            if operator == '*' {
                value *= operand;
            } else if operand == 0.0 {
                return Err("the balance path divides by 0".to_owned());
            } else {
                value /= operand;
            }
        }
        Ok(value)
    }

    fn unary(&mut self) -> std::result::Result<f64, String> {
        self.space();
        let Some(current) = self.peek() else {
            return Err("the balance path ends where a number or a path should be".to_owned());
        };
        match current {
            '-' => {
                self.index += 1;
                Ok(-self.unary()?)
            }
            '(' => {
                self.index += 1;
                let value = self.sum()?;
                self.space();
                match self.peek() {
                    Some(')') => self.index += 1,
                    _ => return Err("the balance path has a ( without its )".to_owned()),
                }
                Ok(value)
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = self.index;
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '.' {
                        self.index += 1;
                    } else {
                        break;
                    }
                }
                let text: String = self.source[start..self.index].iter().collect();
                text.parse::<f64>()
                    .map_err(|_| format!("the balance path has {text:?}, not a number"))
            }
            _ => {
                let start = self.index;
                while let Some(c) = self.peek() {
                    if matches!(c, '+' | '-' | '*' | '/' | '(' | ')' | ' ') {
                        break;
                    }
                    self.index += 1;
                }
                if self.index == start {
                    let rest: String = self.source[start..].iter().collect();
                    return Err(format!(
                        "the balance path has {rest:?} where a number or a path should be"
                    ));
                }
                let path: String = self.source[start..self.index].iter().collect();
                let found = self.at(&path)?;
                number(found).ok_or_else(|| format!("{path:?} in the reply is not a number"))
            }
        }
    }

    // at is what is at a dotted path in the reply.
    fn at(&self, path: &str) -> std::result::Result<&'a Value, String> {
        let mut found = self.reply;
        for segment in path.split('.') {
            found = match found {
                Value::Object(object) => object.get(segment).unwrap_or(&Value::Null),
                Value::Array(array) => segment
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| array.get(index))
                    .unwrap_or(&Value::Null),
                _ => &Value::Null,
            };
            if found.is_null() {
                return Err(format!("nothing at {path:?} in the reply"));
            }
        }
        Ok(found)
    }
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

    #[test]
    fn custom_paths_take_sums_brackets_and_percents() {
        let value = serde_json::json!({"credits": {"monthlyCredits": 42.0, "cap": 70.0}});
        assert_eq!(
            custom_balance(&value, "(1 - credits.monthlyCredits / credits.cap) %").unwrap(),
            "40%"
        );
        assert_eq!(custom_balance(&value, "$credits.cap").unwrap(), "$70.00");
        assert_eq!(
            custom_balance(&value, "-credits.monthlyCredits").unwrap(),
            "-42.00"
        );
    }

    #[test]
    fn custom_paths_take_several_labelled_amounts() {
        let value = serde_json::json!({
            "windowLimits": {"fiveHour": {"used": 0.0, "cap": 100.0}, "weekly": {"used": 3.4, "cap": 100.0}},
            "credits": {"monthlyCredits": 70.0}
        });
        assert_eq!(
            custom_balance(
                &value,
                "5h: windowLimits.fiveHour.used / windowLimits.fiveHour.cap %; week: windowLimits.weekly.used / windowLimits.weekly.cap %; $credits.monthlyCredits"
            )
            .unwrap(),
            "5h 0% · week 3.4% · $70.00"
        );
    }

    #[test]
    fn custom_paths_reject_broken_sums() {
        let value = serde_json::json!({"a": 1.0});
        assert!(custom_balance(&value, "a +").is_err());
        assert!(custom_balance(&value, "(a").is_err());
        assert!(custom_balance(&value, "a / 0").is_err());
        assert!(custom_balance(&value, "b + a").is_err());
    }
}
