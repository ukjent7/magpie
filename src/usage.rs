use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{Mutex, PoisonError},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{agent, settings};

static APPEND_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TokenUsage {
    pub input: usize,
    pub output: usize,
    pub cache_read: usize,
    pub cache_write: usize,
    pub reasoning: usize,
}

impl TokenUsage {
    pub(crate) fn merge(&mut self, other: Self) {
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write = self.cache_write.max(other.cache_write);
        self.reasoning = self.reasoning.max(other.reasoning);
    }
}

#[derive(Clone)]
pub(crate) struct Request {
    agent: String,
    provider: String,
    model: String,
    started: Instant,
}

impl Request {
    pub(crate) fn new(
        user_agent: Option<&str>,
        provider: &str,
        model: &str,
        started: Instant,
    ) -> Self {
        Self {
            agent: agent_of(user_agent.unwrap_or_default()),
            provider: provider.to_owned(),
            model: model.to_owned(),
            started,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Record {
    #[serde(rename = "t")]
    time: String,
    agent: String,
    provider: String,
    model: String,
    #[serde(rename = "in")]
    input: usize,
    out: usize,
    cache_read: usize,
    cache_write: usize,
    reasoning: usize,
    ms: u64,
    status: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Period {
    Today,
    Week,
    Month,
    All,
}

impl Period {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "today" | "day" => Some(Self::Today),
            "7d" | "week" => Some(Self::Week),
            "30d" | "month" => Some(Self::Month),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Week => "last 7 days",
            Self::Month => "last 30 days",
            Self::All => "all time",
        }
    }

    fn since(self, now: OffsetDateTime) -> i64 {
        if matches!(self, Self::All) {
            return i64::MIN;
        }
        let today = now.date().midnight().assume_offset(now.offset());
        match self {
            Self::Today => today.unix_timestamp(),
            Self::Week => (today - Duration::days(6)).unix_timestamp(),
            Self::Month => (today - Duration::days(29)).unix_timestamp(),
            Self::All => i64::MIN,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Totals {
    pub(crate) calls: usize,
    pub(crate) errors: usize,
    pub(crate) input: usize,
    pub(crate) output: usize,
    pub(crate) cache_read: usize,
    pub(crate) cache_write: usize,
    pub(crate) reasoning: usize,
    pub(crate) cost: f64,
    pub(crate) unpriced: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct UsageRow {
    pub(crate) name: String,
    pub(crate) totals: Totals,
    pub(crate) share: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct UsageReport {
    pub(crate) period: Period,
    pub(crate) totals: Totals,
    pub(crate) agents: Vec<UsageRow>,
    pub(crate) models: Vec<UsageRow>,
    pub(crate) path: PathBuf,
}

impl Totals {
    fn add(&mut self, record: &Record, price: Option<crate::catalog::Price>) {
        self.calls += 1;
        self.errors += usize::from(record.status >= 400);
        self.input += record.input;
        self.output += record.out;
        self.cache_read += record.cache_read;
        self.cache_write += record.cache_write;
        self.reasoning += record.reasoning;
        if record.input + record.out == 0 {
            return;
        }
        if let Some(price) = price {
            self.cost += price.cost(
                record.input,
                record.out,
                record.cache_read,
                record.cache_write,
            );
        } else {
            self.unpriced += 1;
        }
    }

    pub(crate) fn tokens(self) -> usize {
        self.input + self.output
    }

    pub(crate) fn display_cost(self) -> String {
        format_cost(self)
    }
}

pub(crate) fn record(request: Request, status: u16, usage: TokenUsage) {
    let Ok(time) = OffsetDateTime::now_utc().format(&Rfc3339) else {
        return;
    };
    let record = Record {
        time,
        agent: request.agent,
        provider: request.provider,
        model: request.model,
        input: usage.input,
        out: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        ms: request.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        status,
    };
    let Ok(mut bytes) = serde_json::to_vec(&record) else {
        return;
    };
    bytes.push(b'\n');

    let _guard = APPEND_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = path();
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(path) {
        let _ = file.write_all(&bytes);
    }
}

pub(crate) fn command(args: &[String]) -> Result<()> {
    let period = match args {
        [] => Period::Month,
        [value] => Period::parse(value).context("usage: magpie usage [today|7d|30d|all]")?,
        _ => bail!("usage: magpie usage [today|7d|30d|all]"),
    };
    let report = report(period);
    let totals = report.totals;

    if totals.calls == 0 {
        println!(
            "no calls {} · route an agent through magpie and its usage shows up here",
            report.period.title()
        );
        println!("  {}", report.path.display());
        return Ok(());
    }

    println!(
        "{} tokens · {} calls · {} errors · {} · {}",
        format_tokens(totals.tokens()),
        totals.calls,
        totals.errors,
        report.period.title(),
        format_cost(totals)
    );
    println!(
        "  in {}  out {}  cache read {}  cache write {}  reasoning {}",
        format_tokens(totals.input),
        format_tokens(totals.output),
        format_tokens(totals.cache_read),
        format_tokens(totals.cache_write),
        format_tokens(totals.reasoning)
    );
    print_groups("agents", &report.agents);
    print_groups("models", &report.models);
    println!("  {}", report.path.display());
    Ok(())
}

pub(crate) fn report(period: Period) -> UsageReport {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let since = period.since(now);
    let records = load();
    let records = records
        .into_iter()
        .filter(|record| {
            OffsetDateTime::parse(&record.time, &Rfc3339)
                .is_ok_and(|time| time.unix_timestamp() >= since)
        })
        .collect::<Vec<_>>();

    let mut totals = Totals::default();
    let mut agents = HashMap::<String, Totals>::new();
    let mut models = HashMap::<String, Totals>::new();
    let mut prices = HashMap::<(String, String), Option<crate::catalog::Price>>::new();
    for record in &records {
        let price = *prices
            .entry((record.provider.clone(), record.model.clone()))
            .or_insert_with(|| {
                crate::provider::catalog_id_for_usage(&record.provider)
                    .and_then(|catalog| crate::catalog::price_of(&catalog, &record.model))
            });
        totals.add(record, price);
        agents
            .entry(record.agent.clone())
            .or_default()
            .add(record, price);
        models
            .entry(format!("{}/{}", record.provider, record.model))
            .or_default()
            .add(record, price);
    }

    let total_tokens = totals.tokens();
    UsageReport {
        period,
        totals,
        agents: usage_rows(agents, total_tokens, |id| agent_name(&id)),
        models: usage_rows(models, total_tokens, |id| id),
        path: path(),
    }
}

fn usage_rows(
    groups: HashMap<String, Totals>,
    total_tokens: usize,
    name: impl Fn(String) -> String,
) -> Vec<UsageRow> {
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(|(left_id, left), (right_id, right)| {
        right
            .tokens()
            .cmp(&left.tokens())
            .then_with(|| right.calls.cmp(&left.calls))
            .then_with(|| left_id.cmp(right_id))
    });
    groups
        .into_iter()
        .map(|(id, totals)| UsageRow {
            name: name(id),
            share: if total_tokens == 0 {
                0.0
            } else {
                100.0 * totals.tokens() as f64 / total_tokens as f64
            },
            totals,
        })
        .collect()
}

fn print_groups(heading: &str, rows: &[UsageRow]) {
    let width = rows
        .iter()
        .map(|row| row.name.len())
        .max()
        .unwrap_or_default();
    println!("\n  {heading}");
    for row in rows {
        println!(
            "  {name:<width$}  {share:>3.0}%  {tokens:>7}  {calls:<10}  {cost}",
            name = row.name.as_str(),
            share = row.share,
            tokens = format_tokens(row.totals.tokens()),
            calls = plural(row.totals.calls, "call"),
            cost = format_cost(row.totals),
        );
    }
}

fn load() -> Vec<Record> {
    let Ok(file) = fs::File::open(path()) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    reader
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

fn path() -> PathBuf {
    settings::providers_path().with_file_name("usage.jsonl")
}

fn agent_of(user_agent: &str) -> String {
    let product = user_agent
        .trim()
        .split(['/', ' '])
        .next()
        .unwrap_or_default();
    if product.is_empty() {
        return "other".to_owned();
    }
    let product = product.to_ascii_lowercase();
    const USER_AGENT_PREFIXES: &[(&str, &str)] = &[
        ("claude-cli", "claude"),
        ("claude-code", "claude"),
        ("codex", "codex"),
        ("geminicli", "gemini"),
        ("gemini-cli", "gemini"),
        ("pi-", "pi"),
        ("github-copilot", "copilot"),
        ("deepseek-harness", "dsh"),
        ("command-code", "commandcode"),
        ("oh-my-pi", "omp"),
        ("hermes-agent", "hermes"),
    ];
    if let Some((_, id)) = USER_AGENT_PREFIXES
        .iter()
        .find(|(prefix, _)| product.starts_with(prefix))
    {
        return (*id).to_owned();
    }
    agent::all()
        .into_iter()
        .find(|agent| agent.spec.id == product || agent.spec.aliases.contains(&product.as_str()))
        .map_or(product, |agent| agent.spec.id.to_owned())
}

fn agent_name(id: &str) -> String {
    agent::all()
        .into_iter()
        .find(|agent| agent.spec.id == id)
        .map_or_else(|| id.to_owned(), |agent| agent.spec.name.to_owned())
}

fn plural(count: usize, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

pub(crate) fn format_tokens(tokens: usize) -> String {
    match tokens {
        1_000_000_000.. => format!("{:.2}B", tokens as f64 / 1_000_000_000.0),
        10_000_000.. => format!("{:.0}M", tokens as f64 / 1_000_000.0),
        1_000_000.. => format!("{:.1}M", tokens as f64 / 1_000_000.0),
        100_000.. => format!("{:.0}K", tokens as f64 / 1_000.0),
        1_000.. => format!("{:.1}K", tokens as f64 / 1_000.0),
        _ => tokens.to_string(),
    }
}

fn format_cost(totals: Totals) -> String {
    if totals.cost == 0.0 && totals.unpriced > 0 {
        return "no price".to_owned();
    }
    let cost = match totals.cost {
        cost if cost >= 100.0 => format!("${cost:.0}"),
        cost if cost >= 1.0 => format!("${cost:.2}"),
        cost => format!("${cost:.3}"),
    };
    if totals.unpriced > 0 {
        format!("≈{cost}+")
    } else {
        format!("≈{cost}")
    }
}
