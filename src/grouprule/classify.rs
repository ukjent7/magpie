// A rule with an intent is for a kind of message: as a user's turn begins,
// the group's classifier model is asked which of the intents the user's
// message is — once, before the turn goes anywhere, and never again within
// it. The call goes through magpie itself, so the classifier is any model
// magpie has, with its keys, failover and usage; it shows in the usage as
// magpie's own call. When it can't say (it fails, is slow, or answers
// something else) no intent matches, and the trace tells why.

use std::{
    fmt::Write as _,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Classifier;

// RouterAgent is the User-Agent magpie's own classifier calls carry.
pub const ROUTER_AGENT: &str = "magpie-router/1";

const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(8);
const CLASSIFY_KEEP: Duration = Duration::from_secs(10 * 60); // an answer, for the same message and intents
const CLASSIFY_REST: Duration = Duration::from_secs(30); // after a failure, before the classifier is asked again
const MAX_CLASSIFIED: usize = 4096;

const CLASSIFY_PROMPT: &str = "You route a user's message to a coding assistant by what it asks for. \
Given numbered kinds of request and the user's message, answer with the number of the kind the message is, \
or 0 if it is none of them. Answer with the number only.";

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build classifier HTTP client")
});

#[derive(Clone)]
struct ClassifiedAs {
    intent: String,
    at: Instant,
}

struct ClassifyFailure {
    error: String,
    at: Instant,
}

static ANSWERED: LazyLock<
    Mutex<(
        std::collections::HashMap<String, ClassifiedAs>,
        std::collections::HashMap<String, ClassifyFailure>,
    )>,
> = LazyLock::new(|| {
    Mutex::new((
        std::collections::HashMap::new(),
        std::collections::HashMap::new(),
    ))
});

// answerError is a classifier that answered, but not with one of the
// numbers: it is up, so it isn't left to rest for it.
#[derive(Debug)]
pub struct AnswerError(String);

impl std::fmt::Display for AnswerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AnswerError {}

fn answer_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AnswerError(message.into()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// classify is ask's answer for the message, from what was asked before
// when it can be: the same message and intents within CLASSIFY_KEEP. A
// classifier that failed is left to rest for CLASSIFY_REST rather than
// making every turn wait out its timeout.
pub async fn classify(
    ask: &dyn Classifier,
    model: &str,
    intents: &[String],
    text: &str,
) -> Result<(String, bool)> {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update([0]);
    hasher.update(intents.join("\x00").to_ascii_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(text.as_bytes());
    let key = hex(&hasher.finalize());
    let now = Instant::now();
    {
        let state = ANSWERED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(saved) = state
            .0
            .get(&key)
            .filter(|saved| now.saturating_duration_since(saved.at) < CLASSIFY_KEEP)
        {
            return Ok((saved.intent.clone(), true));
        }
        if let Some(failed) = state
            .1
            .get(model)
            .filter(|failed| now.saturating_duration_since(failed.at) < CLASSIFY_REST)
        {
            let ago = now.saturating_duration_since(failed.at).as_secs();
            bail!(
                "{model} failed {ago}s ago ({}); not asked again for now",
                failed.error
            );
        }
    }
    let intent = ask.ask(model, intents, text).await;
    let mut state = ANSWERED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match intent {
        Ok(intent) => {
            state.1.remove(model);
            state.0.insert(
                key,
                ClassifiedAs {
                    intent: intent.clone(),
                    at: Instant::now(),
                },
            );
            if state.0.len() > MAX_CLASSIFIED {
                state.0.retain(|_, saved| {
                    Instant::now().saturating_duration_since(saved.at) <= CLASSIFY_KEEP
                });
            }
            Ok((intent, false))
        }
        Err(error) => {
            if error.downcast_ref::<AnswerError>().is_none() {
                state.1.insert(
                    model.to_owned(),
                    ClassifyFailure {
                        error: error.to_string(),
                        at: Instant::now(),
                    },
                );
            }
            Err(error)
        }
    }
}

// classifyBody is the Chat request asking model which of intents text is,
// at effort when it isn't "". A model that reasons does so before it
// answers, and how long varies: 2048 leaves room for that and the number.
pub fn classify_body(model: &str, effort: &str, intents: &[String], text: &str) -> Value {
    let mut kinds = String::from("Kinds:\n");
    for (index, intent) in intents.iter().enumerate() {
        let _ = writeln!(kinds, "{}. {intent}", index + 1);
    }
    let message = format!(
        "{kinds}\nThe user's message:\n<message>\n{text}\n</message>\n\nThe number of its kind (0 for none):"
    );
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": CLASSIFY_PROMPT},
            {"role": "user", "content": message}
        ],
        "stream": false,
        "temperature": 0,
        "max_tokens": 2048
    });
    if !effort.is_empty() {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

// readIntent is the intent a classifier's answer names: its first number,
// 0 for none.
pub fn read_intent(answer: &str, intents: &[String]) -> Result<String> {
    let digits = answer
        .trim_start_matches(|character: char| !character.is_ascii_digit())
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    let number = digits.parse::<i64>().ok();
    match number {
        Some(n) if n >= 0 && n as usize <= intents.len() => {
            if n == 0 {
                Ok(String::new())
            } else {
                Ok(intents[n as usize - 1].clone())
            }
        }
        _ => {
            let mut quoted = answer.trim().chars().take(80).collect::<String>();
            if answer.trim().chars().count() > 80 {
                quoted.push('…');
            }
            Err(answer_error(format!(
                "it answered {quoted:?}, not a number from 0 to {}",
                intents.len()
            )))
        }
    }
}

// fitEffort is the level of the model's own nearest the one asked for — a
// tie goes up — or the one asked for when the model's aren't known.
const EFFORT_RANK: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

pub fn fit_effort(want: &str, levels: &[String]) -> String {
    if levels.is_empty() || levels.iter().any(|level| level == want) {
        return want.to_owned();
    }
    let Some(at) = EFFORT_RANK.iter().position(|level| *level == want) else {
        return want.to_owned();
    };
    let (mut best, mut dist) = (want.to_owned(), EFFORT_RANK.len());
    for level in levels {
        let Some(index) = EFFORT_RANK.iter().position(|name| name == level) else {
            continue;
        };
        if level == "none" {
            continue;
        }
        let distance = index.abs_diff(at);
        if distance < dist || distance == dist && index > at {
            best = level.clone();
            dist = distance;
        }
    }
    best
}

// classifyEffort is the least reasoning model takes: none when it can go
// without, else its lowest level; "" when its levels aren't known, which
// leaves it to the vendor.
pub fn classify_effort(model: &str) -> String {
    let Some((provider_id, model_id)) = model.split_once('/') else {
        return String::new();
    };
    let Some(catalog_id) = crate::provider::catalog_id_for_usage(provider_id) else {
        return String::new();
    };
    let Some(model) = crate::catalog::available_models(provider_id, &catalog_id)
        .into_iter()
        .find(|entry| entry.id == model_id)
    else {
        return String::new();
    };
    if model.efforts.is_empty() {
        return String::new();
    }
    fit_effort("none", &model.efforts)
}

// GatewayClassifier asks the model through the gateway itself, as a client
// would, so the call keeps magpie's routing, keys and usage.
pub struct GatewayClassifier;

impl Classifier for GatewayClassifier {
    fn ask<'a>(
        &'a self,
        model: &'a str,
        intents: &'a [String],
        text: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let body = classify_body(model, &classify_effort(model), intents, text);
            let response = CLIENT
                .post(format!("{}/chat/completions", crate::gateway::v1_url()))
                .header(reqwest::header::USER_AGENT, ROUTER_AGENT)
                .timeout(CLASSIFY_TIMEOUT)
                .json(&body)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) if error.is_timeout() => bail!("{model} gave no answer in 8s"),
                Err(error) => return Err(error).context(format!("{model} did not answer")),
            };
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .with_context(|| format!("{model}: {status}, not an answer"))?;
            let body: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("{model}: {}, not an answer", status.as_u16()))?;
            if let Some(message) = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
            {
                bail!("{model}: {message}");
            }
            if !status.is_success() {
                bail!("{model}: {}", status.canonical_reason().unwrap_or("failed"));
            }
            let Some(content) = body
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
            else {
                return Err(answer_error(format!("{model} gave no answer")));
            };
            read_intent(content, intents)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_with_words_around_the_number_are_read() {
        let intents = vec!["a quick question".to_owned()];
        assert_eq!(
            read_intent("Kind 1.", &intents).unwrap(),
            "a quick question"
        );
    }

    #[test]
    fn odd_answers_do_not_name_an_intent() {
        let intents = vec!["x".to_owned(), "y".to_owned()];
        let error = read_intent("maybe", &intents).unwrap_err();
        assert!(error.to_string().contains("not a number"));
        // the classifier is up for it: it isn't left to rest
        assert!(error.downcast_ref::<AnswerError>().is_some());
    }

    #[test]
    fn the_same_message_and_intents_are_kept() {
        let key = |model: &str, intents: &[String], text: &str| {
            let mut hasher = Sha256::new();
            hasher.update(model.as_bytes());
            hasher.update([0]);
            hasher.update(intents.join("\x00").to_ascii_lowercase().as_bytes());
            hasher.update([0]);
            hasher.update(text.as_bytes());
            hex(&hasher.finalize())
        };
        let intents = vec!["refactoring".to_owned()];
        let first = key("c/cls", &intents, "rename this package");
        let again = key("c/cls", &intents, "rename this package");
        assert_eq!(first, again);
        let other = key("c/cls", &intents, "rename that package");
        assert_ne!(first, other);
        // the intents' case doesn't change what was asked
        assert_eq!(
            key("c/cls", &["Refactoring".to_owned()], "rename this package"),
            first
        );
    }
}
