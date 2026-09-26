use anyhow::Result;
use serde::Serialize;

// magpie quota: what is left of every subscription and key balance, for
// scripts and agents. The gateway answers the same at GET /v1/magpie/quotas
// (only to this machine, as it names the accounts), so an agent can pick
// where to send its work.

const USAGE: &str = "usage: magpie quota [<provider>] [--json]";

#[derive(Clone, Serialize)]
pub(crate) struct QuotaSpan {
    name: String,
    used: f64,
    remaining: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    display: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct Quota {
    provider: String,
    name: String,
    kind: &'static str, // subscription or balance
    #[serde(skip_serializing_if = "String::is_empty")]
    plan: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    user: String,
    windows: Vec<QuotaSpan>,
    #[serde(skip_serializing_if = "String::is_empty")]
    balance: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
}

impl QuotaSpan {
    pub(crate) fn new(
        name: String,
        used: f64,
        remaining: f64,
        resets_at: Option<String>,
        display: String,
    ) -> Self {
        Self {
            name,
            used,
            remaining: remaining.max(0.0),
            resets_at,
            display,
        }
    }
}

impl Quota {
    // subscription is one signed-in account's allowance windows.
    pub(crate) fn subscription(
        provider: String,
        name: String,
        plan: String,
        user: String,
        windows: Vec<QuotaSpan>,
        error: String,
    ) -> Self {
        Self {
            provider,
            name,
            kind: "subscription",
            plan,
            user,
            windows,
            balance: String::new(),
            error,
        }
    }
}

pub(crate) async fn command(args: &[String]) -> Result<()> {
    let mut provider_filter = None;
    let mut as_json = false;
    for argument in args {
        if argument.eq_ignore_ascii_case("--json") {
            as_json = true;
            continue;
        }
        if provider_filter.is_some() {
            bail!("{USAGE}");
        }
        provider_filter = Some(argument.clone());
    }

    let quotas = report().await;
    let quotas = match provider_filter {
        Some(provider) => quotas
            .into_iter()
            .filter(|quota| quota.provider.eq_ignore_ascii_case(&provider))
            .collect::<Vec<_>>(),
        None => quotas,
    };

    if as_json {
        println!("{}", serde_json::to_string_pretty(&quotas)?);
        return Ok(());
    }
    if quotas.is_empty() {
        println!(
            "nothing to report · sign in with magpie accounts add, or set a provider's balance"
        );
        return Ok(());
    }
    for quota in quotas {
        print_quota(&quota);
    }
    Ok(())
}

fn print_quota(quota: &Quota) {
    match quota.kind {
        "subscription" => {
            if quota.windows.is_empty() {
                println!(
                    "{} ({}){}{}",
                    quota.provider,
                    quota.user,
                    if quota.plan.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", quota.plan)
                    },
                    if quota.error.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", quota.error)
                    }
                );
                return;
            }
            println!(
                "{} ({}){}",
                quota.provider,
                quota.user,
                if quota.plan.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", quota.plan)
                }
            );
        }
        _ => {
            println!("{} ({})", quota.provider, quota.name);
            if !quota.balance.is_empty() {
                println!("  balance: {}", quota.balance);
            }
            if !quota.error.is_empty() {
                println!("  {}", quota.error);
            }
            return;
        }
    }
    for window in &quota.windows {
        let resets = window
            .resets_at
            .as_deref()
            .map(|at| format!(" · resets {at}"))
            .unwrap_or_default();
        let display = if window.display.is_empty() {
            String::new()
        } else {
            format!(" ({})", window.display)
        };
        println!(
            "  {:<12} {:>3.0}% used · {:>3.0}% left{display}{resets}",
            window.name, window.used, window.remaining
        );
    }
}

pub(crate) async fn report() -> Vec<Quota> {
    let mut out = crate::accounts::quota_subscriptions().await;
    for (provider, name, balance) in crate::provider::key_balances().await {
        let (balance, error) = match balance {
            Ok(balance) => (balance, String::new()),
            Err(error) => (String::new(), format!("{error:#}")),
        };
        out.push(Quota {
            provider,
            name,
            kind: "balance",
            plan: String::new(),
            user: String::new(),
            windows: Vec::new(),
            balance,
            error,
        });
    }
    out
}
