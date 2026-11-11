//! A provider's model list: fetched by `deckd` from `<base_url>/models` and
//! cached per URL under the data root (ADR 0012 §4).
//!
//! The design choice: the window never fetches and never sees a key. The
//! daemon resolves the provider from its own settings, reads the named
//! credential through the broker for this one request, and keeps only ids and
//! metadata. A failed refetch serves the cached list with the reason beside
//! it, so an offline provider still lets a window configure a session.

use crate::settings::Provider;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a fetched list is used without asking again.
pub const TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// One `/models` request, before giving up.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// A `/models` body this large is not a model list.
const BODY_LIMIT: u64 = 4 * 1024 * 1024;
const CACHE_DIR: &str = "models";

/// What one `/models` entry says about a model. Everything but the id is
/// optional because only some endpoints state it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context: Option<u32>,
    pub max_output: Option<u32>,
    /// USD per million tokens.
    pub prompt_per_m: Option<f64>,
    pub completion_per_m: Option<f64>,
    /// Whether it supports tool calls. An entry that lists no parameters is
    /// assumed to.
    pub tools: bool,
    pub reasoning: bool,
    pub images: bool,
}

/// A provider's list as the settings panel reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelListing {
    pub provider: String,
    pub url: String,
    pub models: Vec<ModelInfo>,
    /// When the served list was fetched; `None` for a hand-written one.
    pub fetched_at: Option<u64>,
    /// True when this is the cached list because the refetch failed.
    pub stale: bool,
    /// Why the refetch failed, when `stale` is set.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    url: String,
    fetched_at: u64,
    models: Vec<ModelInfo>,
}

/// The entries of a `/models` body (`{"data": [...]}` or a bare array), by id.
pub fn parse(body: &Value) -> Vec<ModelInfo> {
    let entries = body.get("data").unwrap_or(body).as_array().map(Vec::as_slice).unwrap_or_default();
    let mut out: Vec<ModelInfo> = entries.iter().filter_map(entry).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

fn entry(e: &Value) -> Option<ModelInfo> {
    let id = e.get("id")?.as_str().filter(|s| !s.is_empty())?.to_string();
    let name = e.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
    let count = |v: Option<&Value>| v.and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
    // OpenRouter prices are USD per token, as strings; a router model's price
    // is "-1" (it depends on the pick), which is unknown, not negative.
    let price = |k: &str| {
        let v = e.pointer(&format!("/pricing/{k}"))?;
        let p = v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64())?;
        (p >= 0.0).then_some(p * 1_000_000.0)
    };
    let params = e.get("supported_parameters").and_then(Value::as_array);
    let has = |p: &str| params.is_some_and(|ps| ps.iter().any(|v| v == p));
    let images = e
        .pointer("/architecture/input_modalities")
        .and_then(Value::as_array)
        .is_some_and(|m| m.iter().any(|v| v == "image"));
    Some(ModelInfo {
        id,
        name,
        // vLLM states the window it serves as `max_model_len`, beside the
        // model's trained `context_length`; the served one is what fits.
        context: count(e.get("max_model_len")).or_else(|| count(e.get("context_length"))),
        max_output: count(e.pointer("/top_provider/max_completion_tokens")),
        prompt_per_m: price("prompt"),
        completion_per_m: price("completion"),
        tools: params.is_none() || has("tools"),
        reasoning: has("reasoning"),
        images,
    })
}

/// The provider's list: fresh cache, else a fetch (which also stores the
/// cache), else the cache with the fetch's reason beside it.
pub fn listing(data: &Path, provider: &Provider, key: Option<&str>, refresh: bool) -> Result<ModelListing> {
    let now = now();
    let cached = read_cache(data, &provider.url);
    if !refresh
        && let Some(cached) = &cached
        && now.saturating_sub(cached.fetched_at) < TTL.as_secs()
    {
        return Ok(served(provider, cached, false, None));
    }
    match fetch(&provider.url, key) {
        Ok(models) => {
            let cached = Cached { url: provider.url.clone(), fetched_at: now, models };
            if let Err(e) = write_cache(data, &cached) {
                eprintln!("deckd: keeping {}'s model list in memory only: {e:#}", provider.name);
            }
            Ok(served(provider, &cached, false, None))
        }
        Err(e) => match cached {
            Some(cached) => Ok(served(provider, &cached, true, Some(format!("{e:#}")))),
            None => Err(e).with_context(|| format!("listing {}'s models", provider.name)),
        },
    }
}

fn served(provider: &Provider, cached: &Cached, stale: bool, error: Option<String>) -> ModelListing {
    ModelListing {
        provider: provider.name.clone(),
        url: provider.url.clone(),
        models: cached.models.clone(),
        fetched_at: (cached.fetched_at > 0).then_some(cached.fetched_at),
        stale,
        error,
    }
}

/// `GET {base_url}/models`. Only this module ever holds the key value.
fn fetch(url: &str, key: Option<&str>) -> Result<Vec<ModelInfo>> {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        // A redirect could leave the saved provider.
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut request = agent.get(format!("{}/models", url.trim_end_matches('/')));
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let mut response = request.call().context("model list request failed")?;
    if !response.status().is_success() {
        bail!("model list returned {}", response.status());
    }
    let body = response.body_mut().with_config().limit(BODY_LIMIT).read_to_vec()?;
    let value: Value = serde_json::from_slice(&body).context("model list is not JSON")?;
    let models = parse(&value);
    if models.is_empty() {
        bail!("model list has no models");
    }
    Ok(models)
}

fn read_cache(data: &Path, url: &str) -> Option<Cached> {
    let bytes = std::fs::read(cache_path(data, url)).ok()?;
    let cached: Cached = serde_json::from_slice(&bytes).ok()?;
    (cached.url == url).then_some(cached)
}

fn write_cache(data: &Path, cached: &Cached) -> Result<()> {
    let dir = data.join(CACHE_DIR);
    std::fs::create_dir_all(&dir)?;
    crate::vm::write_atomic(&cache_path(data, &cached.url), &serde_json::to_vec(cached)?)
}

/// One file per URL: the same endpoint serves the same list to every provider
/// that names it, and editing a provider's URL simply uses another file.
fn cache_path(data: &Path, url: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(url.as_bytes());
    let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    data.join(CACHE_DIR).join(format!("{hex}.json"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |age| age.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn ids(listing: &ModelListing) -> Vec<&str> {
        listing.models.iter().map(|model| model.id.as_str()).collect()
    }

    fn provider(url: &str) -> Provider {
        Provider { name: "local".into(), url: url.into(), secret: Some("LOCAL_API_KEY".into()), models: Vec::new() }
    }

    fn data(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-catalog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn body(ids: &[&str]) -> String {
        json!({ "data": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>() }).to_string()
    }

    /// A local `/v1/models` answering one response per entry, recording each
    /// request head.
    fn server(answers: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for (status, body) in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                seen.lock().unwrap().push(head);
                let reason = if status == 200 { "OK" } else { "Server Error" };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, requests)
    }

    #[test]
    fn a_model_list_reads_openrouter_and_local_entries() {
        let body = json!({ "data": [
            {
                "id": "anthropic/claude-sonnet-5-5",
                "name": "Claude Sonnet",
                "context_length": 200000,
                "pricing": { "prompt": "0.000003", "completion": "0.000015" },
                "supported_parameters": ["tools", "reasoning"],
                "architecture": { "input_modalities": ["text", "image"] },
                "top_provider": { "max_completion_tokens": 64000 }
            },
            { "id": "qwen3-32b", "max_model_len": 32768 },
            { "id": "router/x", "pricing": { "prompt": "-1", "completion": "-1" } },
            { "name": "no id" },
            { "id": "qwen3-32b", "name": "duplicate" }
        ] });
        let models = parse(&body);
        assert_eq!(
            models.iter().map(|model| model.id.as_str()).collect::<Vec<_>>(),
            ["anthropic/claude-sonnet-5-5", "qwen3-32b", "router/x"]
        );
        let sonnet = &models[0];
        assert_eq!((sonnet.context, sonnet.max_output), (Some(200000), Some(64000)));
        assert_eq!((sonnet.prompt_per_m, sonnet.completion_per_m), (Some(3.0), Some(15.0)));
        assert!(sonnet.tools && sonnet.reasoning && sonnet.images);
        let qwen = &models[1];
        assert_eq!(qwen.context, Some(32768));
        assert!(qwen.tools, "an entry that states no parameters is assumed to call tools");
        assert_eq!((models[2].prompt_per_m, models[2].completion_per_m), (None, None), "-1 is unknown, not free");

        assert_eq!(parse(&json!([{ "id": "bare" }])).len(), 1, "a bare array is a list");
    }

    #[test]
    fn a_fetched_list_is_cached_per_url_and_refresh_asks_again() {
        let (url, seen) = server(vec![(200, body(&["a", "b"])), (200, body(&["a", "b", "c"]))]);
        let data = data("cache");
        let provider = provider(&url);
        let first = listing(&data, &provider, Some("sk-test"), false).unwrap();
        assert_eq!(ids(&first), ["a", "b"]);
        assert_eq!((first.provider.as_str(), first.url.as_str()), ("local", url.as_str()));
        assert!(first.fetched_at.is_some() && !first.stale && first.error.is_none());

        // The fresh cache answers without a request.
        let again = listing(&data, &provider, None, false).unwrap();
        assert_eq!(ids(&again), ["a", "b"]);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(
            seen.lock().unwrap()[0].to_lowercase().contains("authorization: bearer sk-test"),
            "{:?}",
            seen.lock().unwrap()[0]
        );

        // Refresh fetches again; a keyless refresh sends no key.
        let refreshed = listing(&data, &provider, None, true).unwrap();
        assert_eq!(ids(&refreshed), ["a", "b", "c"]);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(!seen.lock().unwrap()[1].to_lowercase().contains("authorization"));
    }

    #[test]
    fn a_failed_refetch_serves_the_cached_list_with_its_reason() {
        let (url, _) = server(vec![(200, body(&["a"])), (500, "{}".into())]);
        let data = data("stale");
        let provider = provider(&url);
        assert_eq!(ids(&listing(&data, &provider, None, false).unwrap()), ["a"]);
        let fallback = listing(&data, &provider, None, true).unwrap();
        assert!(fallback.stale);
        assert_eq!(ids(&fallback), ["a"]);
        assert!(fallback.error.unwrap_or_default().contains("500"));
    }

    #[test]
    fn a_first_fetch_that_fails_is_an_error() {
        let (url, _) = server(vec![(500, "{}".into())]);
        let error = format!("{:#}", listing(&data("failure"), &provider(&url), None, false).unwrap_err());
        assert!(error.contains("500"), "{error}");
    }
}
