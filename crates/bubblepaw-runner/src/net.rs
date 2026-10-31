//! Bounded, read-only web capabilities shared by CLI and embedded hosts.
use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{Store, Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use regex::Regex;
use reqwest::{Url, header};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

mod cache;
#[cfg(test)]
mod deepseek_tests;
mod passages;
mod sources;
use crate::provider::LiveProviderConfig;
use cache::SqliteCache;
pub use passages::Passage;
pub use sources::Source;
use sources::SourceStore;

const BODY_LIMIT: usize = 5_000_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
// Crawl4AI renders the page in a browser before answering, so it gets a longer bound.
const CRAWL4AI_TIMEOUT: Duration = Duration::from_secs(60);
const RESULT_LIMIT: usize = 60 * 1024;
// Longest cooldown a Retry-After header can impose.
const MAX_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEEPSEEK_SEARCH_BASE_URL: &str = "https://api.deepseek.com/v1/tools";

/// Accepts either one value or an array, so a single-string setting stays valid.
pub fn one_or_many<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        Many(Vec<T>),
        One(T),
    }
    Ok(match OneOrMany::<T>::deserialize(deserializer)? {
        OneOrMany::Many(values) => values,
        OneOrMany::One(value) => vec![value],
    })
}

/// The backend answered with HTTP 429 or the service refused the request for rate reasons.
/// Carries the Retry-After hint when one was given.
#[derive(Debug)]
pub(crate) struct RateLimited {
    message: String,
    retry_after: Option<Duration>,
}
impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RateLimited {}

/// The backend answered with a CAPTCHA or challenge page instead of its results.
#[derive(Debug)]
pub(crate) struct Blocked(&'static str);
impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Blocked {}

/// Cooldown requested by a failure: `Some(None)` means the configured default,
/// `Some(Some(d))` a Retry-After hint. `None` means no cooldown.
fn cooldown_request(error: &anyhow::Error) -> Option<Option<Duration>> {
    if let Some(limited) = error.downcast_ref::<RateLimited>() {
        return Some(limited.retry_after);
    }
    error.downcast_ref::<Blocked>().map(|_| None)
}

/// Process-wide cooldowns keyed by backend identity, like the response cache.
fn cooldowns() -> &'static Mutex<HashMap<String, Instant>> {
    static COOLDOWNS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    COOLDOWNS.get_or_init(Mutex::default)
}
fn cooling_down(identity: &str) -> Option<Duration> {
    let now = Instant::now();
    let mut cooldowns = cooldowns().lock().expect("web cooldown mutex poisoned");
    cooldowns.retain(|_, until| *until > now);
    cooldowns
        .get(identity)
        .map(|until| until.saturating_duration_since(now))
}
fn start_cooldown(identity: &str, duration: Duration) {
    if duration.is_zero() {
        return;
    }
    cooldowns()
        .lock()
        .expect("web cooldown mutex poisoned")
        .insert(
            identity.to_owned(),
            Instant::now() + duration.min(MAX_COOLDOWN),
        );
}

/// Retry-After from the provider parser, capped at a day for web backends.
fn cooldown_retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    crate::provider::parse_retry_after(headers.get(header::RETRY_AFTER)?.to_str().ok()?)
        .map(|wait| wait.min(MAX_COOLDOWN))
}

/// A challenge page (Cloudflare, CAPTCHA) in a small body, not a real article.
fn challenge_page(body: &[u8]) -> bool {
    if body.len() > 20_000 {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "just a moment...",
        "cf-challenge",
        "captcha",
        "are you a robot",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn search_label(backend: &SearchBackend) -> String {
    match backend {
        SearchBackend::SearxNg { url } => format!("searxng:{url}"),
        SearchBackend::Brave { .. } => "brave".into(),
        SearchBackend::DeepSeek { .. } => "deepseek".into(),
        SearchBackend::DuckDuckGo => "ddg".into(),
    }
}
/// Cooldown and cache identity; API keys appear only as digests.
fn search_identity(backend: &SearchBackend) -> String {
    match backend {
        SearchBackend::Brave { api_key } => {
            format!(
                "brave:{}",
                bubblepaw_core::hex_lower(Sha256::digest(api_key.as_bytes()))
            )
        }
        SearchBackend::DeepSeek { base_url, api_key } => format!(
            "deepseek:{base_url}:{}",
            bubblepaw_core::hex_lower(Sha256::digest(api_key.as_bytes()))
        ),
        other => search_label(other),
    }
}
fn fetch_label(backend: &FetchBackend) -> String {
    match backend {
        FetchBackend::Direct => "direct".into(),
        FetchBackend::Crawl4Ai { url } => format!("crawl4ai:{url}"),
    }
}
/// Fetch cache identity: direct and Crawl4AI entries stay apart, as do two Crawl4AI servers.
fn fetch_identity(backend: &FetchBackend) -> String {
    match backend {
        FetchBackend::Direct => "fetch".into(),
        FetchBackend::Crawl4Ai { url } => format!("crawl4ai:{url}"),
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum SearchBackend {
    #[serde(rename = "deepseek")]
    DeepSeek {
        base_url: String,
        api_key: String,
    },
    SearxNg {
        url: String,
    },
    Brave {
        api_key: String,
    },
    DuckDuckGo,
}
impl Default for SearchBackend {
    fn default() -> Self {
        Self::DeepSeek {
            base_url: DEEPSEEK_SEARCH_BASE_URL.into(),
            api_key: String::new(),
        }
    }
}
impl fmt::Debug for SearchBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeepSeek { .. } => {
                f.write_str("DeepSeek { base_url: [configured], api_key: [redacted] }")
            }
            Self::SearxNg { .. } => f.write_str("SearxNg { url: [configured] }"),
            Self::Brave { .. } => f.write_str("Brave { api_key: [redacted] }"),
            Self::DuckDuckGo => f.write_str("DuckDuckGo"),
        }
    }
}

/// Where `web_fetch` reads pages: the harness itself, or a Crawl4AI Docker server.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum FetchBackend {
    #[default]
    Direct,
    /// Base URL of a Crawl4AI server (its `/crawl` route is used).
    #[serde(rename = "crawl4ai")]
    Crawl4Ai { url: String },
}

pub use crate::config::WebExpose;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Ordered search backends; a call tries them in turn. Must not be empty.
    #[serde(deserialize_with = "one_or_many")]
    pub search: Vec<SearchBackend>,
    /// Ordered fetch backends; a call tries them in turn. Must not be empty.
    #[serde(deserialize_with = "one_or_many")]
    pub fetch: Vec<FetchBackend>,
    pub expose: WebExpose,
    pub allow_private: bool,
    pub user_agent: String,
    pub cache_ttl_secs: u64,
    /// Maximum serialized response/key bytes in the disposable SQLite cache.
    pub cache_max_bytes: usize,
    /// Only actual cache misses spend the optional per-handle budget.
    pub max_searches: Option<usize>,
    pub max_fetches: Option<usize>,
    /// Seconds a backend that rate-limited or blocked is skipped; 0 disables cooldowns.
    pub cooldown_secs: u64,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            search: vec![SearchBackend::default(), SearchBackend::DuckDuckGo],
            fetch: vec![FetchBackend::default()],
            expose: WebExpose::default(),
            allow_private: false,
            user_agent: "BubblePaw/0.1".into(),
            cache_ttl_secs: 86_400,
            cache_max_bytes: 64 * 1024 * 1024,
            max_searches: None,
            max_fetches: None,
            cooldown_secs: 300,
        }
    }
}

/// Prepared backend choices published with the model-provider generation.
/// Cache, budgets, exposure and address policy stay on the host's client.
pub struct NetBackendSnapshot {
    search: Vec<SearchBackend>,
    fetch: Vec<FetchBackend>,
    cooldown_secs: u64,
    crawl4ai_token: Option<String>,
}
impl fmt::Debug for NetBackendSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetBackendSnapshot")
            .field("search_backend_count", &self.search.len())
            .field("fetch_backend_count", &self.fetch.len())
            .field("cooldown_secs", &self.cooldown_secs)
            .field("crawl4ai_token_configured", &self.crawl4ai_token.is_some())
            .finish()
    }
}
impl NetBackendSnapshot {
    pub fn from_config(config: Option<&WebConfig>) -> Result<Self> {
        Self::from_config_with_env(config, &|name| std::env::var(name).ok())
    }
    pub fn from_config_with_env(
        config: Option<&WebConfig>,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let Some(config) = config else {
            return Ok(Self {
                search: Vec::new(),
                fetch: Vec::new(),
                cooldown_secs: 0,
                crawl4ai_token: None,
            });
        };
        ensure!(
            !config.search.is_empty(),
            "web search needs at least one backend"
        );
        ensure!(
            !config.fetch.is_empty(),
            "web fetch needs at least one backend"
        );
        for backend in &config.search {
            if let SearchBackend::DeepSeek { base_url, api_key } = backend {
                checked_url(base_url)?;
                if !api_key.trim().is_empty() {
                    header::HeaderValue::from_str(&format!("Bearer {api_key}"))
                        .map_err(|_| anyhow!("invalid DeepSeek API key header"))?;
                }
            }
            if let SearchBackend::SearxNg { url } = backend {
                checked_url(url)?;
            }
            if let SearchBackend::Brave { api_key } = backend
                && !api_key.trim().is_empty()
            {
                header::HeaderValue::from_str(api_key)
                    .map_err(|_| anyhow!("invalid Brave API key header"))?;
            }
        }
        for backend in &config.fetch {
            if let FetchBackend::Crawl4Ai { url } = backend {
                checked_url(url)?;
            }
        }
        let crawl4ai_token = config
            .fetch
            .iter()
            .any(|backend| matches!(backend, FetchBackend::Crawl4Ai { .. }))
            .then(|| non_blank_token(environment("CRAWL4AI_API_TOKEN")))
            .flatten();
        if let Some(token) = &crawl4ai_token {
            header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| anyhow!("invalid Crawl4AI API token header"))?;
        }
        Ok(Self {
            search: config.search.clone(),
            fetch: config.fetch.clone(),
            cooldown_secs: config.cooldown_secs,
            crawl4ai_token,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResult {
    /// Root-chat source ID, present on store-aware tool replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Original immutable source mapping (the final fetched URL may redirect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub url: String,
    /// The fetch backend that served the page (`direct` or `crawl4ai:<url>`).
    pub backend: String,
    pub title: String,
    pub content: String,
    pub total_chars: usize,
    /// Stable structural passage numbers. Paging returns overlapping excerpts;
    /// a query returns the top BM25 passages in relevance order.
    pub passages: Vec<Passage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}
/// Search results with the backend that served them (`deepseek`, `ddg`, `searxng:<url>` or `brave`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOutcome {
    pub backend: String,
    pub results: Vec<SearchResult>,
}
/// A search answer from the chain: the first backend with results, or the first
/// backend that answered with none when nothing better was found.
#[derive(Clone, Serialize, Deserialize)]
struct SearchServed {
    backend: String,
    results: Vec<SearchResult>,
}

#[derive(Clone)]
pub struct NetClient {
    config: WebConfig,
    live: Option<Arc<LiveProviderConfig>>,
    backend_snapshot: Option<Arc<NetBackendSnapshot>>,
    budget: Arc<Budget>,
    cache: SqliteCache,
    sources: Option<SourceStore>,
}
#[derive(Default)]
struct Budget {
    searches: AtomicUsize,
    fetches: AtomicUsize,
}
#[derive(Clone)]
struct Page {
    url: Url,
    media_type: String,
    body: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ReadablePage {
    url: String,
    title: String,
    content: String,
    passages: Vec<Passage>,
    total_chars: usize,
}
impl ReadablePage {
    fn convert(page: Page) -> Result<Self> {
        let text = String::from_utf8_lossy(&page.body);
        let (title, content) = if matches!(
            page.media_type.as_str(),
            "text/html" | "application/xhtml+xml"
        ) {
            html_content(&text)?
        } else if page.media_type.starts_with("text/")
            || page.media_type == "application/json"
            || page.media_type.ends_with("+json")
        {
            (String::new(), text.into_owned())
        } else {
            bail!("unsupported web content type");
        };
        let passages = passages::split(&content);
        let total_chars = content.chars().count();
        Ok(Self {
            url: page.url.to_string(),
            title,
            content,
            passages,
            total_chars,
        })
    }
}

/// The fields of Crawl4AI's `/crawl` reply that the harness uses.
#[derive(Deserialize)]
struct CrawlReply {
    success: bool,
    #[serde(default)]
    results: Vec<CrawlResultReply>,
}
#[derive(Deserialize)]
struct CrawlResultReply {
    success: bool,
    #[serde(default)]
    redirected_url: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    markdown: Option<CrawlMarkdownReply>,
}
#[derive(Deserialize)]
struct CrawlMarkdownReply {
    #[serde(default)]
    fit_markdown: Option<String>,
}

// Versioned keys partition backend, address policy and user agent. IDs never
// enter shared cache payloads.
#[derive(Serialize)]
struct CacheKey<'a> {
    version: u8,
    kind: &'a str,
    target: &'a str,
    allow_private: bool,
    user_agent: &'a str,
}
#[allow(deprecated)] // fetch_update supports the workspace's Rust 1.88 MSRV.
fn spend(counter: &AtomicUsize, limit: Option<usize>, name: &str) -> Result<()> {
    let Some(limit) = limit else {
        return Ok(());
    };
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            (used < limit).then(|| used + 1)
        })
        .map(|_| ())
        .map_err(|_| anyhow!("web {name} budget exhausted (limit {limit})"))
}

impl NetClient {
    pub fn new(config: &WebConfig) -> Result<Self> {
        ensure!(
            config.max_searches != Some(0),
            "web max_searches must be positive"
        );
        ensure!(
            config.max_fetches != Some(0),
            "web max_fetches must be positive"
        );
        ensure!(
            config.cache_max_bytes > 0,
            "web cache_max_bytes must be positive"
        );
        header::HeaderValue::from_str(&config.user_agent)
            .map_err(|_| anyhow!("invalid web user agent"))?;
        ensure!(
            !config.search.is_empty(),
            "web search needs at least one backend"
        );
        ensure!(
            !config.fetch.is_empty(),
            "web fetch needs at least one backend"
        );
        for backend in &config.search {
            if let SearchBackend::DeepSeek { base_url, .. } = backend {
                checked_url(base_url)?;
            }
            if let SearchBackend::SearxNg { url } = backend {
                checked_url(url)?;
            }
        }
        for backend in &config.fetch {
            if let FetchBackend::Crawl4Ai { url } = backend {
                checked_url(url)?;
            }
        }
        let cache = SqliteCache::temporary_shared()?;
        cache.enforce_limit(config.cache_max_bytes)?;
        Ok(Self {
            config: config.clone(),
            live: None,
            backend_snapshot: None,
            budget: Arc::new(Budget::default()),
            cache,
            sources: None,
        })
    }

    /// Bind the cache to host data_dir and persist source IDs in root chats.
    pub fn with_store(config: &WebConfig, store: &Store) -> Result<Self> {
        let mut client = Self::new(config)?;
        if let Some(directory) = store.data_dir() {
            client.cache = SqliteCache::open(directory.join("web-cache.db"))?;
            client.cache.enforce_limit(config.cache_max_bytes)?;
        }
        client.sources = Some(SourceStore::new(store));
        Ok(client)
    }

    /// Keep the existing cache, source IDs and miss budgets across backend updates.
    pub fn with_store_live(
        config: &WebConfig,
        store: &Store,
        live: Arc<LiveProviderConfig>,
    ) -> Result<Self> {
        let mut client = Self::with_store(config, store)?;
        client.live = Some(live);
        Ok(client)
    }

    fn request_client(&self) -> Self {
        let mut client = self.clone();
        if let Some(live) = &self.live {
            let snapshot = live.snapshot();
            client.config.search = snapshot.net.search.clone();
            client.config.fetch = snapshot.net.fetch.clone();
            client.config.cooldown_secs = snapshot.net.cooldown_secs;
            client.config.allow_private = snapshot.settings.web.allow_private;
            client.config.user_agent = snapshot.settings.web.user_agent.clone();
            client.config.max_searches = snapshot.settings.web.max_searches;
            client.config.max_fetches = snapshot.settings.web.max_fetches;
            client.backend_snapshot = Some(snapshot.net.clone());
        }
        client.live = None;
        client
    }

    fn crawl4ai_token(&self) -> Option<String> {
        match &self.backend_snapshot {
            Some(snapshot) => snapshot.crawl4ai_token.clone(),
            None => crawl4ai_token(),
        }
    }

    /// Explicit cache path for embedders without a directory Store.
    pub fn with_cache_path(config: &WebConfig, path: impl AsRef<Path>) -> Result<Self> {
        let mut client = Self::new(config)?;
        client.cache = SqliteCache::open(path)?;
        client.cache.enforce_limit(config.cache_max_bytes)?;
        Ok(client)
    }

    /// Reclaim the disposable cache WAL when the host is idle.
    pub fn checkpoint_cache(&self) -> Result<()> {
        self.cache.checkpoint()
    }

    pub async fn fetch(&self, url: &str, offset: usize, max_chars: usize) -> Result<FetchResult> {
        self.fetch_selected(url, offset, max_chars, None, 5).await
    }

    /// Optional local BM25 retrieval over deterministic numbered passages.
    pub async fn fetch_selected(
        &self,
        url: &str,
        offset: usize,
        max_chars: usize,
        query: Option<&str>,
        top_k: usize,
    ) -> Result<FetchResult> {
        self.request_client()
            .fetch_selected_fixed(url, offset, max_chars, query, top_k)
            .await
    }

    async fn fetch_selected_fixed(
        &self,
        url: &str,
        offset: usize,
        max_chars: usize,
        query: Option<&str>,
        top_k: usize,
    ) -> Result<FetchResult> {
        ensure!(max_chars > 0, "max_chars must be positive");
        ensure!((1..=20).contains(&top_k), "top_k must be between 1 and 20");
        if let Some(query) = query {
            ensure!(!query.trim().is_empty(), "query must not be empty");
            ensure!(query.len() <= 16 * 1024, "query is too long");
            ensure!(
                offset == 0,
                "query cannot be combined with a nonzero offset"
            );
        }
        let mut url = checked_url(url)?;
        url.set_fragment(None);
        let (page, backend) = self.fetch_chain(&url).await?;
        ensure!(offset <= page.total_chars, "offset exceeds total_chars");
        let mut result = FetchResult {
            id: None,
            source: None,
            url: page.url.clone(),
            backend,
            title: page.title.clone(),
            content: String::new(),
            total_chars: page.total_chars,
            passages: Vec::new(),
            next_offset: None,
        };
        if let Some(query) = query {
            result.passages = passages::rank(&page.passages, query, top_k);
            while !result.passages.is_empty() {
                result.content = result
                    .passages
                    .iter()
                    .map(|p| format!("[passage {}]\n{}", p.number, p.text))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if serde_json::to_vec(&result)?.len() <= RESULT_LIMIT - 128
                    && result.content.chars().count() <= max_chars
                {
                    break;
                }
                result.passages.pop();
            }
            if result.passages.is_empty() {
                result.content.clear();
            }
        } else {
            let candidate: String = page
                .content
                .chars()
                .skip(offset)
                .take(max_chars.min(RESULT_LIMIT))
                .collect();
            let count = candidate.chars().count();
            let fits = |taken: usize, result: &mut FetchResult| -> Result<bool> {
                result.content = candidate.chars().take(taken).collect();
                result.passages = passages::overlapping(&page.passages, offset, offset + taken);
                result.next_offset = (offset + taken < page.total_chars).then_some(offset + taken);
                Ok(serde_json::to_vec(result)?.len() <= RESULT_LIMIT - 128)
            };
            let mut low = 0;
            let mut high = count;
            ensure!(fits(0, &mut result)?, "web metadata exceeds result limit");
            while low < high {
                let middle = low + (high - low).div_ceil(2);
                if fits(middle, &mut result)? {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            fits(low, &mut result)?;
        }
        Ok(result)
    }

    pub async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        Ok(self.search_served(query, count).await?.results)
    }

    pub async fn search_served(&self, query: &str, count: usize) -> Result<SearchOutcome> {
        self.request_client()
            .search_served_fixed(query, count)
            .await
    }

    async fn search_served_fixed(&self, query: &str, count: usize) -> Result<SearchOutcome> {
        ensure!(!query.trim().is_empty(), "query must not be empty");
        ensure!(query.len() <= 16 * 1024, "query is too long");
        ensure!((1..=20).contains(&count), "count must be between 1 and 20");
        let query = query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let served = self.search_chain(&query).await?;
        let mut outcome = SearchOutcome {
            backend: served.backend,
            results: bounded_search(served.results.into_iter().take(count).collect())?,
        };
        bounded_search_outcome(&mut outcome)?;
        Ok(outcome)
    }

    fn cache_key(&self, kind: &str, target: &str) -> Result<String> {
        Ok(serde_json::to_string(&CacheKey {
            version: 1,
            kind,
            target,
            allow_private: self.config.allow_private,
            user_agent: &self.config.user_agent,
        })?)
    }

    async fn lookup<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        key: &str,
    ) -> Result<Option<T>> {
        self.cache
            .lookup_async(
                key,
                Duration::from_secs(self.config.cache_ttl_secs),
                self.config.cache_max_bytes,
            )
            .await
    }

    async fn insert<T: Serialize + Send + 'static>(&self, key: &str, value: T) -> Result<()> {
        self.cache
            .insert_async(
                key,
                value,
                Duration::from_secs(self.config.cache_ttl_secs),
                self.config.cache_max_bytes,
            )
            .await
    }

    /// Hits precede cooldown/budgets. Singleflight waiters recheck the cache;
    /// only the caller starting network requests spends once per logical call.
    async fn search_chain(&self, query: &str) -> Result<SearchServed> {
        let mut empty = None;
        for backend in &self.config.search {
            let key = self.cache_key(&search_identity(backend), query)?;
            if let Some(served) = self.lookup::<SearchServed>(&key).await? {
                if !served.results.is_empty() {
                    return Ok(served);
                }
                empty.get_or_insert(served);
            }
        }
        let mut spent = false;
        let mut failures = Vec::new();
        for backend in &self.config.search {
            let label = search_label(backend);
            if let SearchBackend::DeepSeek { api_key, .. } = backend
                && api_key.trim().is_empty()
            {
                tracing::warn!(target: "bubblepaw::web", "deepseek search skipped: no API key (set web.deepseek_key_env)");
                failures.push(format!("{label}: skipped, no API key"));
                continue;
            }
            if let SearchBackend::Brave { api_key } = backend
                && api_key.trim().is_empty()
            {
                tracing::warn!(target: "bubblepaw::web", "brave search skipped: no API key (set web.key_env)");
                failures.push(format!("{label}: skipped, no API key"));
                continue;
            }
            let identity = search_identity(backend);
            let key = self.cache_key(&identity, query)?;
            let flight = cache::flight(self.cache.path(), &key);
            let _guard = flight.lock().await;
            if let Some(served) = self.lookup::<SearchServed>(&key).await? {
                if !served.results.is_empty() {
                    return Ok(served);
                }
                empty.get_or_insert(served);
                continue;
            }
            if let Some(remaining) = cooling_down(&identity) {
                failures.push(format!(
                    "{label}: cooling down for another {} s",
                    remaining.as_secs().max(1)
                ));
                continue;
            }
            if !spent {
                spend(&self.budget.searches, self.config.max_searches, "search")?;
                spent = true;
            }
            match self.search_backend(backend, query).await {
                Ok(results) => {
                    let served = SearchServed {
                        backend: label.clone(),
                        results,
                    };
                    self.insert(&key, served.clone()).await?;
                    if !served.results.is_empty() {
                        return Ok(served);
                    }
                    failures.push(format!("{label}: no results"));
                    empty.get_or_insert(served);
                }
                Err(error) => {
                    if let Some(hint) = cooldown_request(&error) {
                        start_cooldown(
                            &identity,
                            hint.unwrap_or(Duration::from_secs(self.config.cooldown_secs)),
                        );
                    }
                    failures.push(format!("{label}: {error}"));
                }
            }
        }
        match empty {
            Some(served) => Ok(served),
            None => bail!("all web search backends failed: {}", failures.join("; ")),
        }
    }

    async fn search_backend(
        &self,
        backend: &SearchBackend,
        query: &str,
    ) -> Result<Vec<SearchResult>> {
        if let SearchBackend::DeepSeek { base_url, api_key } = backend {
            return bounded_search(self.deepseek_search(base_url, api_key, query).await?);
        }
        let (mut url, exempt, key) = match backend {
            SearchBackend::DeepSeek { .. } => unreachable!(),
            SearchBackend::SearxNg { url } => {
                let mut url = checked_url(url)?;
                url.set_path(&format!("{}/search", url.path().trim_end_matches('/')));
                url.set_query(None);
                url.query_pairs_mut().append_pair("format", "json");
                (url, true, None)
            }
            SearchBackend::Brave { api_key } => {
                ensure!(
                    !api_key.trim().is_empty(),
                    "Brave search key is not configured"
                );
                (
                    checked_url("https://api.search.brave.com/res/v1/web/search")?,
                    false,
                    Some(api_key.as_str()),
                )
            }
            SearchBackend::DuckDuckGo => (
                checked_url("https://html.duckduckgo.com/html/")?,
                false,
                None,
            ),
        };
        url.query_pairs_mut().append_pair("q", query);
        if key.is_some() {
            url.query_pairs_mut().append_pair("count", "20");
        }
        let page = self.get(url, exempt, key).await?;
        let results = match backend {
            SearchBackend::DeepSeek { .. } => unreachable!(),
            SearchBackend::SearxNg { .. } => parse_json_search(&page.body, false, 20)?,
            SearchBackend::Brave { .. } => parse_json_search(&page.body, true, 20)?,
            SearchBackend::DuckDuckGo => parse_ddg(&String::from_utf8_lossy(&page.body), 20)?,
        };
        bounded_search(results)
    }

    /// DeepSeek's search API returns snippets directly; it needs no model call.
    /// Reject redirects so the bearer credential never reaches another endpoint.
    async fn deepseek_search(
        &self,
        base_url: &str,
        api_key: &str,
        query: &str,
    ) -> Result<Vec<SearchResult>> {
        let mut endpoint = checked_url(base_url)?;
        endpoint.set_path(&format!("{}/search", endpoint.path().trim_end_matches('/')));
        endpoint.set_query(None);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(&self.config.user_agent)
            .build()
            .map_err(|_| anyhow!("cannot construct DeepSeek search client"))?;
        let mut response = client
            .post(endpoint)
            .bearer_auth(api_key)
            .json(&json!({"query": query, "count": 20, "need_content": false}))
            .send()
            .await
            .map_err(|_| anyhow!("DeepSeek search request failed"))?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(anyhow::Error::new(RateLimited {
                message: "DeepSeek search returned HTTP 429 (rate limited)".into(),
                retry_after: cooldown_retry_after(response.headers()),
            }));
        }
        ensure!(
            status.is_success(),
            "DeepSeek search returned HTTP {}",
            status.as_u16()
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("cannot read DeepSeek search response"))?
        {
            ensure!(
                body.len().saturating_add(chunk.len()) <= BODY_LIMIT,
                "DeepSeek search response exceeds 5 MB body limit"
            );
            body.extend_from_slice(&chunk);
        }
        parse_deepseek_search(&body, 20)
    }

    /// Try each fetch backend in order and return the first page served, with its label.
    /// A challenge page counts as blocked. Only a Crawl4AI service's own HTTP 429 starts a
    /// cooldown: a 429 or challenge from a target site concerns that page, not the backend.
    async fn fetch_chain(&self, url: &Url) -> Result<(ReadablePage, String)> {
        for backend in &self.config.fetch {
            let key = self.cache_key(&fetch_identity(backend), url.as_str())?;
            if let Some(page) = self.lookup::<ReadablePage>(&key).await? {
                return Ok((page, fetch_label(backend)));
            }
        }
        let mut failures = Vec::new();
        let mut spent = false;
        for backend in &self.config.fetch {
            let label = fetch_label(backend);
            let identity = fetch_identity(backend);
            let key = self.cache_key(&identity, url.as_str())?;
            let flight = cache::flight(self.cache.path(), &key);
            let _guard = flight.lock().await;
            if let Some(page) = self.lookup::<ReadablePage>(&key).await? {
                return Ok((page, label));
            }
            if let Some(remaining) = cooling_down(&identity) {
                failures.push(format!(
                    "{label}: cooling down for another {} s",
                    remaining.as_secs().max(1)
                ));
                continue;
            }
            if !spent {
                spend(&self.budget.fetches, self.config.max_fetches, "fetch")?;
                spent = true;
            }
            let attempt = async {
                let page = match backend {
                    FetchBackend::Direct => self.get(url.clone(), false, None).await?,
                    FetchBackend::Crawl4Ai { url: service } => {
                        self.crawl4ai_page(service, url, self.crawl4ai_token().as_deref())
                            .await?
                    }
                };
                if challenge_page(&page.body) {
                    return Err(anyhow::Error::new(Blocked("CAPTCHA or challenge page")));
                }
                let page = ReadablePage::convert(page)?;
                self.insert(&key, page.clone()).await?;
                Ok(page)
            }
            .await;
            match attempt {
                Ok(page) => return Ok((page, label)),
                Err(error) => {
                    if let FetchBackend::Crawl4Ai { .. } = backend
                        && let Some(limited) = error.downcast_ref::<RateLimited>()
                    {
                        start_cooldown(
                            &identity,
                            limited
                                .retry_after
                                .unwrap_or(Duration::from_secs(self.config.cooldown_secs)),
                        );
                    }
                    failures.push(format!("{label}: {error}"));
                }
            }
        }
        bail!("all web fetch backends failed: {}", failures.join("; "))
    }

    /// Ask a Crawl4AI server for the pruned ("fit") Markdown of `target` with POST
    /// `<service>/crawl`, which reports the final URL after redirects. The service URL is
    /// operator configuration, so its own request skips the address check as SearXNG's
    /// does. The target is checked before Crawl4AI fetches it, and the final URL after it.
    async fn crawl4ai_page(
        &self,
        service: &str,
        target: &Url,
        token: Option<&str>,
    ) -> Result<Page> {
        if !self.config.allow_private {
            let addresses = resolve(target).await?;
            ensure!(!addresses.is_empty(), "web host has no addresses");
            ensure!(
                addresses.iter().all(|address| public_ip(address.ip())),
                "web URL resolves to a prohibited address"
            );
        }
        let mut endpoint = checked_url(service)?;
        endpoint.set_path(&format!("{}/crawl", endpoint.path().trim_end_matches('/')));
        endpoint.set_query(None);
        // Pruned Markdown; bypass Crawl4AI's own cache because this client caches responses.
        let body = serde_json::to_vec(&json!({
            "urls": [target.as_str()],
            "crawler_config": {
                "type": "CrawlerRunConfig",
                "params": {
                    "cache_mode": "bypass",
                    "markdown_generator": {
                        "type": "DefaultMarkdownGenerator",
                        "params": {"content_filter": {"type": "PruningContentFilter", "params": {}}}
                    }
                }
            }
        }))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(CRAWL4AI_TIMEOUT)
            .user_agent(&self.config.user_agent)
            .build()
            .map_err(|_| anyhow!("cannot construct crawl4ai client"))?;
        let mut request = client
            .post(endpoint)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        // The token is only ever sent as a header; it is never logged or put in an error.
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                anyhow!("crawl4ai fetch backend timed out after 60 seconds")
            } else {
                anyhow!("crawl4ai fetch backend is unreachable")
            }
        })?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(anyhow::Error::new(RateLimited {
                message: "crawl4ai fetch backend returned HTTP 429 (rate limited)".into(),
                retry_after: cooldown_retry_after(response.headers()),
            }));
        }
        ensure!(
            status.is_success(),
            "crawl4ai fetch backend returned HTTP {}",
            status.as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("crawl4ai fetch backend response could not be read"))?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= BODY_LIMIT,
                "crawl4ai fetch backend response exceeds 5 MB body limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let reply: CrawlReply = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("crawl4ai fetch backend returned an invalid response"))?;
        ensure!(
            reply.success,
            "crawl4ai fetch backend reported a failed crawl"
        );
        let result = reply
            .results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("crawl4ai fetch backend returned an invalid response"))?;
        // Check the final URL before any content is used, even if the crawl reported failure:
        // Crawl4AI fetches redirects itself, so content may come from an internal address.
        let final_url = match result.redirected_url.as_deref() {
            Some(value) => checked_url(value)
                .map_err(|_| anyhow!("crawl4ai fetch backend reported an unusable final URL"))?,
            None => target.clone(),
        };
        if !self.config.allow_private && final_url != *target {
            ensure!(
                public_destination(&final_url).await?,
                "crawl4ai fetch backend followed a redirect to a prohibited address"
            );
        }
        ensure!(
            result.success,
            "crawl4ai fetch backend could not crawl the page: {}",
            result
                .error_message
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>()
        );
        let markdown = result
            .markdown
            .and_then(|markdown| markdown.fit_markdown)
            .unwrap_or_default();
        Ok(Page {
            url: final_url,
            media_type: "text/markdown".into(),
            body: markdown.into_bytes(),
        })
    }

    async fn get(&self, url: Url, exempt: bool, key: Option<&str>) -> Result<Page> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.get_inner(url, exempt, key))
            .await
            .map_err(|_| anyhow!("web request timed out after 20 seconds"))?
    }
    async fn get_inner(&self, mut url: Url, exempt: bool, key: Option<&str>) -> Result<Page> {
        for hop in 0..=5 {
            let host = url.host_str().ok_or_else(|| anyhow!("URL needs a host"))?;
            let addresses = resolve(&url).await?;
            ensure!(!addresses.is_empty(), "web host has no addresses");
            // Exemption applies only to the configured search request, never redirects.
            if !self.config.allow_private && !(exempt && hop == 0) {
                ensure!(
                    addresses.iter().all(|address| public_ip(address.ip())),
                    "web URL resolves to a prohibited address"
                );
            }
            // Disable environment proxies and pin the already-validated DNS answers.
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(REQUEST_TIMEOUT)
                .user_agent(&self.config.user_agent)
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|_| anyhow!("cannot construct web client"))?;
            let mut request = client.get(url.clone());
            if hop == 0
                && let Some(key) = key
            {
                request = request.header("X-Subscription-Token", key);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| anyhow!("web request failed"))?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(hop < 5, "web redirect limit exceeded");
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| anyhow!("redirect has no valid location"))?;
                let next = url
                    .join(location)
                    .map_err(|_| anyhow!("invalid redirect URL"))?;
                url = checked_url(next.as_str())?;
                continue;
            }
            if response.status().as_u16() == 429 {
                return Err(anyhow::Error::new(RateLimited {
                    message: "web server returned HTTP 429 (rate limited)".into(),
                    retry_after: cooldown_retry_after(response.headers()),
                }));
            }
            if !response.status().is_success()
                && response
                    .headers()
                    .get("cf-mitigated")
                    .and_then(|value| value.to_str().ok())
                    == Some("challenge")
            {
                return Err(anyhow::Error::new(Blocked("CAPTCHA or challenge page")));
            }
            ensure!(
                response.status().is_success(),
                "web server returned HTTP {}",
                response.status().as_u16()
            );
            ensure!(
                response
                    .content_length()
                    .is_none_or(|size| size <= BODY_LIMIT as u64),
                "web response exceeds 5 MB body limit"
            );
            let media_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow!("cannot read web response"))?
            {
                ensure!(
                    body.len().saturating_add(chunk.len()) <= BODY_LIMIT,
                    "web response exceeds 5 MB body limit"
                );
                body.extend_from_slice(&chunk);
            }
            return Ok(Page {
                url,
                media_type,
                body,
            });
        }
        unreachable!()
    }
}

/// True when every address the URL's host resolves to is public.
async fn public_destination(url: &Url) -> Result<bool> {
    let addresses = resolve(url).await?;
    ensure!(!addresses.is_empty(), "web host has no addresses");
    Ok(addresses.iter().all(|address| public_ip(address.ip())))
}

/// Optional bearer token for a Crawl4AI server that requires one (`CRAWL4AI_API_TOKEN`).
fn crawl4ai_token() -> Option<String> {
    non_blank_token(std::env::var("CRAWL4AI_API_TOKEN").ok())
}
fn non_blank_token(raw: Option<String>) -> Option<String> {
    raw.filter(|token| !token.trim().is_empty())
}

fn checked_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow!("invalid web URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "web URL must use http or https"
    );
    ensure!(url.host().is_some(), "web URL needs a host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "web URLs with credentials are not supported"
    );
    Ok(url)
}
async fn resolve(url: &Url) -> Result<Vec<SocketAddr>> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("URL needs a port"))?;
    let host = url.host_str().ok_or_else(|| anyhow!("URL needs a host"))?;
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(vec![SocketAddr::new(ip, port)])
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map(|addresses| addresses.collect())
            .map_err(|_| anyhow!("cannot resolve web host"))
    }
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(matches!(a, 0 | 10 | 127)
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && matches!(b, 18 | 19))
                || ip.is_documentation())
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_ip(ip.into());
            }
            let s = ip.segments();
            // Keep transition/translation mechanisms from tunnelling a denied
            // IPv4 destination (NAT64, Teredo, 6to4). Ordinary global unicast
            // currently lives in 2000::/3; mapped addresses were checked above.
            !(s[0] & 0xe000 != 0x2000
                || s[0] == 0x2002
                || (s[0] == 0x2001 && s[1] < 0x0200)
                || ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || s[0] & 0xfe00 == 0xfc00
                || s[0] & 0xffc0 == 0xfe80
                || s[0] & 0xffc0 == 0xfec0
                || s[0] == 0
                || (s[0] == 0x2001 && s[1] == 0xdb8))
        }
    }
}

fn html_content(html: &str) -> Result<(String, String)> {
    let title_re = Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>")?;
    let title = title_re
        .captures(html)
        .map(|c| html_text(&c[1]))
        .transpose()?
        .unwrap_or_default();
    let mut clean = html.to_owned();
    for tag in ["script", "style", "nav", "title"] {
        clean = Regex::new(&format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>"))?
            .replace_all(&clean, "")
            .into_owned();
    }
    let content = html2text::from_read(clean.as_bytes(), 120)
        .map_err(|_| anyhow!("cannot convert web HTML"))?;
    Ok((title, content.trim().to_owned()))
}
fn html_text(html: &str) -> Result<String> {
    html2text::config::plain_no_decorate()
        .string_from_read(html.as_bytes(), 1000)
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .map_err(|_| anyhow!("cannot read search HTML"))
}
fn parse_json_search(body: &[u8], brave: bool, count: usize) -> Result<Vec<SearchResult>> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| anyhow!("search server returned invalid JSON"))?;
    let results = if brave {
        value.pointer("/web/results")
    } else {
        value.get("results")
    };
    let results = results
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("search JSON has no results array"))?;
    Ok(results
        .iter()
        .filter_map(|entry| {
            let url = entry.get("url")?.as_str()?;
            checked_url(url).ok()?;
            Some(SearchResult {
                id: None,
                title: entry.get("title")?.as_str()?.to_owned(),
                url: url.to_owned(),
                snippet: entry
                    .get(if brave { "description" } else { "content" })
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                published_at: None,
            })
        })
        .take(count)
        .collect())
}
fn parse_deepseek_search(body: &[u8], count: usize) -> Result<Vec<SearchResult>> {
    #[derive(Deserialize)]
    struct Reply {
        results: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        title: String,
        url: String,
        snippet: Option<String>,
        date: Option<String>,
    }
    let reply: Reply = serde_json::from_slice(body)
        .map_err(|_| anyhow!("DeepSeek search returned an invalid response"))?;
    Ok(reply
        .results
        .into_iter()
        .filter_map(|entry| {
            checked_url(&entry.url).ok()?;
            Some(SearchResult {
                id: None,
                title: entry.title,
                url: entry.url,
                snippet: entry.snippet.unwrap_or_default(),
                published_at: entry.date.filter(|date| !date.is_empty()),
            })
        })
        .take(count)
        .collect())
}
fn parse_ddg(html: &str, count: usize) -> Result<Vec<SearchResult>> {
    let anchors = Regex::new(
        r#"(?is)<a\b([^>]*\bclass\s*=\s*["'][^"']*\bresult__a\b[^"']*["'][^>]*)>(.*?)</a\s*>"#,
    )?;
    let href = Regex::new(r#"(?is)\bhref\s*=\s*["']([^"']+)["']"#)?;
    let snippet = Regex::new(
        r#"(?is)<(?:a|div|span)\b[^>]*\bclass\s*=\s*["'][^"']*\bresult__snippet\b[^"']*["'][^>]*>(.*?)</(?:a|div|span)\s*>"#,
    )?;
    let matches: Vec<_> = anchors.captures_iter(html).collect();
    let mut results = Vec::new();
    for (index, capture) in matches.iter().enumerate() {
        let Some(link) = href.captures(&capture[1]) else {
            continue;
        };
        let link = html_text(&link[1])?;
        let link = if link.starts_with("//") {
            format!("https:{link}")
        } else {
            link
        };
        let Ok(mut url) = checked_url(&link) else {
            continue;
        };
        if url
            .host_str()
            .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
            && let Some(target) = url
                .query_pairs()
                .find_map(|(key, value)| (key == "uddg").then(|| value.into_owned()))
        {
            let Ok(target) = checked_url(&target) else {
                continue;
            };
            url = target;
        }
        let start = capture.get(0).expect("full match").end();
        let end = matches
            .get(index + 1)
            .and_then(|c| c.get(0))
            .map_or(html.len(), |m| m.start());
        let snippet = snippet
            .captures(&html[start..end])
            .map(|c| html_text(&c[1]))
            .transpose()?
            .unwrap_or_default();
        results.push(SearchResult {
            id: None,
            title: html_text(&capture[2])?,
            url: url.into(),
            snippet,
            published_at: None,
        });
        if results.len() == count {
            break;
        }
    }
    // DuckDuckGo answers a bot check with a page that has no results, not an error status.
    if results.is_empty() {
        let lower = html.to_ascii_lowercase();
        if ["anomaly-modal", "bots use duckduckgo", "captcha"]
            .iter()
            .any(|marker| lower.contains(marker))
        {
            return Err(anyhow::Error::new(Blocked("CAPTCHA or challenge page")));
        }
    }
    Ok(results)
}
fn bounded_search(mut results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
    for result in &mut results {
        result.title = result.title.chars().take(1000).collect();
        result.snippet = result.snippet.chars().take(3000).collect();
    }
    while serde_json::to_vec(&results)?.len() > RESULT_LIMIT {
        results.pop();
    }
    Ok(results)
}

/// Bound after attaching immutable source metadata. Character paging must keep
/// its continuation correct; query replies retain complete ranked passages.
fn bound_fetch_reply(result: &mut FetchResult, offset: usize, query: bool) -> Result<()> {
    if serde_json::to_vec(result)?.len() <= RESULT_LIMIT {
        return Ok(());
    }
    if query {
        while serde_json::to_vec(result)?.len() > RESULT_LIMIT && !result.passages.is_empty() {
            result.passages.pop();
            result.content = result
                .passages
                .iter()
                .map(|p| format!("[passage {}]\n{}", p.number, p.text))
                .collect::<Vec<_>>()
                .join("\n\n");
        }
        ensure!(
            serde_json::to_vec(result)?.len() <= RESULT_LIMIT,
            "web metadata exceeds result limit"
        );
        return Ok(());
    }
    let content = std::mem::take(&mut result.content);
    let passages = std::mem::take(&mut result.passages);
    let mut low = 0;
    let mut high = content.chars().count();
    let fits = |taken: usize, result: &mut FetchResult| -> Result<bool> {
        result.content = content.chars().take(taken).collect();
        result.passages = passages::overlapping(&passages, offset, offset + taken);
        result.next_offset = (offset + taken < result.total_chars).then_some(offset + taken);
        Ok(serde_json::to_vec(result)?.len() <= RESULT_LIMIT)
    };
    ensure!(fits(0, result)?, "web metadata exceeds result limit");
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(middle, result)? {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    ensure!(
        low > 0 || offset == result.total_chars,
        "web metadata leaves no room for page content"
    );
    fits(low, result)?;
    Ok(())
}

fn bounded_search_outcome(result: &mut SearchOutcome) -> Result<()> {
    while serde_json::to_vec(result)?.len() > RESULT_LIMIT && !result.results.is_empty() {
        result.results.pop();
    }
    ensure!(
        serde_json::to_vec(result)?.len() <= RESULT_LIMIT,
        "web metadata exceeds result limit"
    );
    Ok(())
}

struct NetTool {
    client: NetClient,
    search: bool,
}
#[async_trait]
impl Tool for NetTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, input_schema) = if self.search {
            (
                "web_search",
                "Search the web. Returns {backend,results:[{id?,title,url,snippet}]}. Store-aware hosts persist short source IDs (s1, s2, ...) shared with this chat's children. Search snippets are leads; fetch sources before citing them.",
                json!({"type":"object","properties":{"query":{"type":"string"},"count":{"type":"integer","minimum":1,"maximum":20,"default":8}},"required":["query"],"additionalProperties":false}),
            )
        } else {
            (
                "web_fetch",
                "Read a URL or saved source id as Markdown/text. Returns {id?,source?:{id,url,title,backend},url,backend,title,content,total_chars,passages:[{number,text,start_offset,end_offset}],next_offset?}. source is the original saved mapping; url is the final fetched URL. Passages have stable page-local numbers; cite source ID and passage number. Optional query retrieves top_k passages using local BM25. Otherwise offsets page Unicode characters; follow next_offset. Cached reads use no request budget.",
                json!({"type":"object","properties":{"url":{"type":"string"},"id":{"type":"string"},"query":{"type":"string"},"top_k":{"type":"integer","minimum":1,"maximum":20,"default":5},"offset":{"type":"integer","minimum":0,"default":0},"max_chars":{"type":"integer","minimum":1,"default":20000}},"oneOf":[{"required":["url"]},{"required":["id"]}],"additionalProperties":false}),
            )
        };
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema,
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        let execute = async {
            if self.search {
                let query = input
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("query must be a string"))?;
                let mut result = self
                    .client
                    .search_served(query, number(&input, "count", 8)?)
                    .await?;
                if let Some(sources) = &self.client.sources {
                    for entry in &mut result.results {
                        entry.id = Some(
                            sources
                                .allocate(
                                    &context.session_id,
                                    &entry.url,
                                    &entry.title,
                                    &result.backend,
                                )?
                                .id,
                        );
                    }
                }
                bounded_search_outcome(&mut result)?;
                Ok(ToolResult::text(serde_json::to_string(&result)?))
            } else {
                ensure!(
                    input.get("url").is_some() != input.get("id").is_some(),
                    "provide exactly one of url or id"
                );
                let saved = if let Some(id) = input.get("id") {
                    let id = id.as_str().ok_or_else(|| anyhow!("id must be a string"))?;
                    let sources = self.client.sources.as_ref().ok_or_else(|| {
                        anyhow!("source IDs require store-aware web tool registration")
                    })?;
                    Some(sources.lookup(&context.session_id, id)?)
                } else {
                    None
                };
                let url = if let Some(saved) = &saved {
                    saved.url.as_str()
                } else {
                    input
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("url must be a string"))?
                };
                let query = input
                    .get("query")
                    .map(|value| {
                        value
                            .as_str()
                            .ok_or_else(|| anyhow!("query must be a string"))
                    })
                    .transpose()?;
                let mut result = self
                    .client
                    .fetch_selected(
                        url,
                        number(&input, "offset", 0)?,
                        number(&input, "max_chars", 20_000)?,
                        query,
                        number(&input, "top_k", 5)?,
                    )
                    .await?;
                // Keep the requested URL's ID across redirects. Final response
                // URL names the page read; immutable mapping names its lead.
                if let Some(sources) = &self.client.sources {
                    let source = match &saved {
                        Some(source) => source.clone(),
                        None => sources.allocate(
                            &context.session_id,
                            url,
                            &result.title,
                            &result.backend,
                        )?,
                    };
                    result.id = Some(source.id.clone());
                    result.source = Some(source);
                }
                bound_fetch_reply(&mut result, number(&input, "offset", 0)?, query.is_some())?;
                Ok(ToolResult::text(serde_json::to_string(&result)?))
            }
        };
        tokio::select! { biased; _ = context.cancellation.cancelled() => bail!("web request cancelled"), result = execute => result }
    }
}
fn number(input: &Value, field: &str, default: usize) -> Result<usize> {
    input.get(field).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| anyhow!("{field} must be a nonnegative integer"))
    })
}
/// Register the exposed fetch/search host capabilities (research is registered separately).
pub fn register_fetch_search_tools(registry: &mut ToolRegistry, config: &WebConfig) -> Result<()> {
    register_sources(
        registry,
        NetClient::new(config)?,
        config.expose.fetch,
        config.expose.search,
    )
}
/// Persist root-chat source IDs and share data_dir/web-cache.db across sessions.
pub fn register_fetch_search_tools_with_store(
    registry: &mut ToolRegistry,
    config: &WebConfig,
    store: &Store,
) -> Result<()> {
    register_sources(
        registry,
        NetClient::with_store(config, store)?,
        config.expose.fetch,
        config.expose.search,
    )
}
/// Store-aware web tools whose next request sees the current backend generation.
pub fn register_fetch_search_tools_with_store_live(
    registry: &mut ToolRegistry,
    config: &WebConfig,
    store: &Store,
    live: Arc<LiveProviderConfig>,
) -> Result<()> {
    register_sources(
        registry,
        NetClient::with_store_live(config, store, live)?,
        config.expose.fetch,
        config.expose.search,
    )
}
/// Give a research child both source tools regardless of parent exposure settings.
pub fn register_research_sources(registry: &mut ToolRegistry, config: &WebConfig) -> Result<()> {
    register_sources(registry, NetClient::new(config)?, true, true)
}
/// Store-aware counterpart for a child's source tools.
pub fn register_research_sources_with_store(
    registry: &mut ToolRegistry,
    config: &WebConfig,
    store: &Store,
) -> Result<()> {
    register_sources(registry, NetClient::with_store(config, store)?, true, true)
}
/// Give research both live web tools, including tools hidden from the parent.
pub fn register_research_sources_with_store_live(
    registry: &mut ToolRegistry,
    config: &WebConfig,
    store: &Store,
    live: Arc<LiveProviderConfig>,
) -> Result<()> {
    register_sources(
        registry,
        NetClient::with_store_live(config, store, live)?,
        true,
        true,
    )
}
pub(crate) fn register_sources(
    registry: &mut ToolRegistry,
    client: NetClient,
    fetch: bool,
    search: bool,
) -> Result<()> {
    for is_search in [false, true] {
        if (is_search && search) || (!is_search && fetch) {
            let name = if is_search { "web_search" } else { "web_fetch" };
            registry.register_host(Arc::new(NetTool {
                client: client.clone(),
                search: is_search,
            }))?;
            registry.show_always([name]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    #[test]
    fn crawl_credentials_belong_to_the_request_environment() {
        let config = WebConfig {
            fetch: vec![FetchBackend::Crawl4Ai {
                url: "https://example.test".into(),
            }],
            ..Default::default()
        };
        let client = NetBackendSnapshot::from_config_with_env(Some(&config), &|name| {
            (name == "CRAWL4AI_API_TOKEN").then(|| "client-token".into())
        })
        .unwrap();
        let absent = NetBackendSnapshot::from_config_with_env(Some(&config), &|_| None).unwrap();
        assert_eq!(client.crawl4ai_token.as_deref(), Some("client-token"));
        assert!(absent.crawl4ai_token.is_none());
    }

    #[test]
    fn live_backend_debug_never_contains_credentials() {
        let snapshot = NetBackendSnapshot {
            search: vec![SearchBackend::Brave {
                api_key: "fixture-private-search-key".into(),
            }],
            fetch: vec![FetchBackend::Direct],
            cooldown_secs: 300,
            crawl4ai_token: Some("fixture-private-crawl-token".into()),
        };
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("fixture-private-search-key"));
        assert!(!debug.contains("fixture-private-crawl-token"));
        assert!(debug.contains("crawl4ai_token_configured: true"));
    }

    #[test]
    fn live_backend_prepare_rejects_invalid_brave_header_without_the_key() {
        let config = WebConfig {
            search: vec![SearchBackend::Brave {
                api_key: "fixture-private-key\ninvalid".into(),
            }],
            ..Default::default()
        };
        let error = NetBackendSnapshot::from_config(Some(&config)).unwrap_err();
        assert_eq!(error.to_string(), "invalid Brave API key header");
    }

    #[test]
    fn conservative_public_address_rules() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.2.3.4",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "224.0.0.1",
            "::",
            "::1",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2001::7f00:1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn configuration_defaults_and_debug_redaction() {
        let config: WebConfig = serde_json::from_str("{}").unwrap();
        assert!(config.expose.fetch && config.expose.search);
        assert!(!config.allow_private);
        assert_eq!(config.cache_ttl_secs, 86_400);
        assert_eq!((config.max_searches, config.max_fetches), (None, None));
        assert_eq!(config.cache_max_bytes, 64 * 1024 * 1024);
        let config = WebConfig {
            search: vec![SearchBackend::Brave {
                api_key: "synthetic-key-never-print".into(),
            }],
            ..config
        };
        assert!(!format!("{config:?}").contains("synthetic-key-never-print"));
        let backend: SearchBackend =
            serde_json::from_str(r#"{"backend":"searx_ng","url":"http://127.0.0.1:1"}"#).unwrap();
        assert!(matches!(backend, SearchBackend::SearxNg { .. }));
    }

    #[test]
    fn final_reply_bounds_include_escaped_source_and_search_metadata() {
        let content = "\"🐾\\\n".repeat(20_000);
        let page = ReadablePage {
            url: "https://example.org/".into(),
            title: "\"🐾".repeat(500),
            total_chars: content.chars().count(),
            passages: passages::split(&content),
            content: content.clone(),
        };
        let source = Source {
            id: "s123456".into(),
            url: format!("https://example.org/{}", "long".repeat(1000)),
            title: page.title.clone(),
            backend: "direct".into(),
        };
        let mut result = FetchResult {
            id: Some(source.id.clone()),
            source: Some(source),
            url: page.url,
            title: page.title,
            backend: "direct".into(),
            total_chars: page.total_chars,
            content,
            passages: page.passages,
            next_offset: None,
        };
        bound_fetch_reply(&mut result, 0, false).unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= RESULT_LIMIT);
        assert_eq!(result.next_offset, Some(result.content.chars().count()));
        assert_eq!(
            result.passages.last().unwrap().end_offset,
            result.next_offset.unwrap()
        );
        let mut search = SearchOutcome {
            backend: "backend".repeat(1000),
            results: (0..20)
                .map(|n| SearchResult {
                    id: Some(format!("s{n}")),
                    title: "\"🐾".repeat(500),
                    url: "https://example.org/".into(),
                    snippet: "\\\n🐾".repeat(1000),
                    published_at: Some("2026-10-18".into()),
                })
                .collect(),
        };
        bounded_search_outcome(&mut search).unwrap();
        assert!(serde_json::to_vec(&search).unwrap().len() <= RESULT_LIMIT);
        assert!(!search.results.is_empty());
    }

    #[tokio::test]
    async fn fifty_store_aware_fetches_measure_cache_record_and_pagination_writes() {
        use bubblepaw_core::CancellationToken;
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(directory.path()).unwrap();
        let session = store.create_session("cache measurement").unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let app = Router::new().route(
            "/{page}",
            get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::Relaxed);
                    (
                        [("content-type", "text/html")],
                        format!(
                            "<title>Cache evidence</title><h1>Measured source</h1><p>{}</p>",
                            "Readable paragraph with supporting fixture evidence. ".repeat(200)
                        ),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = NetClient::with_store(
            &WebConfig {
                allow_private: true,
                ..Default::default()
            },
            &store,
        )
        .unwrap();
        let tool = NetTool {
            client: client.clone(),
            search: false,
        };
        let before = store.storage_stats().unwrap();
        let context = || ToolContext {
            session_id: session.id.clone(),
            run_id: "fixture".into(),
            invocation_id: "fetch".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        };
        let mut maximum_wal = 0;
        for index in 0..50 {
            let reply = tool
                .invoke(
                    context(),
                    json!({"url":format!("{base}/p{index}"),"max_chars":100}),
                )
                .await
                .unwrap();
            assert!(
                reply
                    .content
                    .contains(&format!("\"id\":\"s{}\"", index + 1))
            );
            maximum_wal = maximum_wal.max(client.cache.measurements().unwrap().2);
        }
        let stats = client.cache.measurements().unwrap();
        let cached = rusqlite::Connection::open(client.cache.path()).unwrap();
        let payload: Vec<u8> = cached
            .query_row("SELECT payload FROM responses LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let payload = String::from_utf8(payload).unwrap();
        assert!(payload.contains("passages") && payload.contains("Readable paragraph"));
        assert!(
            !payload.contains("<h1>")
                && !payload.contains("<p>")
                && !payload.contains("\"id\"")
                && !payload.contains(&session.id)
        );
        drop(cached);
        let record = store.storage_stats().unwrap();
        assert_eq!(hits.load(Ordering::Relaxed), 50);
        assert_eq!(stats.3, 50);
        assert_eq!(
            record.chat_transaction_count - before.chat_transaction_count,
            50
        );
        for index in 1..=50 {
            tool.invoke(
                context(),
                json!({"id":format!("s{index}"),"offset":100,"max_chars":100}),
            )
            .await
            .unwrap();
        }
        assert_eq!(hits.load(Ordering::Relaxed), 50);
        assert_eq!(client.cache.measurements().unwrap().3, 50);
        assert_eq!(
            store.storage_stats().unwrap().chat_transaction_count,
            record.chat_transaction_count
        );
        client.checkpoint_cache().unwrap();
        let after = client.cache.measurements().unwrap();
        eprintln!(
            "web-cache 50 local store-aware fetches: payload={} db_before={} wal_max_sampled={} db_after={} wal_after={} cache_app_writes={} cache_maintenance_writes={} chat_writes={} catalog_writes={} serving_writes={}; 50 pagination hits add zero HTTP/cache/chat writes",
            stats.0,
            stats.1,
            maximum_wal,
            after.1,
            after.2,
            stats.3,
            stats.4,
            record.chat_transaction_count - before.chat_transaction_count,
            record.catalog_transaction_count - before.catalog_transaction_count,
            record.serving_transaction_count - before.serving_transaction_count
        );
        assert!(after.1 <= after.0 as u64 + 256 * 1024);
        assert_eq!(after.2, 0);
        drop(tool);
        drop(client);
        drop(store);
        let cache_path = directory.path().join("web-cache.db");
        std::fs::remove_file(&cache_path).unwrap();
        let reopened = Store::open_data_dir(directory.path()).unwrap();
        let replacement = NetClient::with_store(
            &WebConfig {
                allow_private: true,
                ..Default::default()
            },
            &reopened,
        )
        .unwrap();
        let tool = NetTool {
            client: replacement,
            search: false,
        };
        let reply = tool
            .invoke(context(), json!({"id":"s7","max_chars":100}))
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&reply.content).unwrap();
        assert_eq!(reply["id"], "s7");
        assert_eq!(reply["source"]["url"], format!("{base}/p6"));
        assert_eq!(hits.load(Ordering::Relaxed), 51);
        server.abort();
    }

    #[tokio::test]
    async fn second_root_reuses_cache_and_preserves_query_passage_numbers() {
        use bubblepaw_core::CancellationToken;
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(directory.path()).unwrap();
        let first = store.create_session("first root").unwrap();
        let second = store.create_session("second root").unwrap();
        let fetches = Arc::new(AtomicUsize::new(0));
        let searches = Arc::new(AtomicUsize::new(0));
        let fetch_counter = fetches.clone();
        let search_counter = searches.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let page_url = format!("{base}/page");
        let search_url = page_url.clone();
        let app = Router::new()
            .route("/page", get(move || {
                let count = fetch_counter.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    let body: String = (1..=10).map(|number| format!("<h1>Section {number}</h1><p>{}</p>",
                        if number == 7 { "Platypus aqueduct evidence. ".repeat(50) }
                        else { "Ordinary explanatory background. ".repeat(50) })).collect();
                    ([ ("content-type", "text/html") ], format!("<title>Stable passages</title>{body}"))
                }
            }))
            .route("/search", get(move || {
                let count = search_counter.clone();
                let url = search_url.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    axum::Json(json!({"results":[{"title":"Saved lead title","url":url,"content":"Read the actual source."}]}))
                }
            }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = WebConfig {
            allow_private: true,
            search: vec![SearchBackend::SearxNg { url: base }],
            max_searches: Some(1),
            max_fetches: Some(1),
            ..Default::default()
        };
        let mut original_passages = None;
        for session in [&first.id, &second.id] {
            let mut registry = ToolRegistry::new();
            register_fetch_search_tools_with_store(&mut registry, &config, &store).unwrap();
            let context = || ToolContext {
                session_id: session.clone(),
                run_id: "fixture".into(),
                invocation_id: "source".into(),
                cancellation: CancellationToken::new(),
                approval: Default::default(),
            };
            let searched = registry
                .get_host("web_search")
                .unwrap()
                .invoke(context(), json!({"query":"second root cache fixture"}))
                .await
                .unwrap();
            let searched: Value = serde_json::from_str(&searched.content).unwrap();
            assert_eq!(searched["results"][0]["id"], "s1");
            let fetched = registry
                .get_host("web_fetch")
                .unwrap()
                .invoke(context(), json!({"id":"s1"}))
                .await
                .unwrap();
            let fetched: Value = serde_json::from_str(&fetched.content).unwrap();
            assert_eq!(fetched["source"]["url"], page_url);
            assert_eq!(fetched["source"]["title"], "Saved lead title");
            let query = json!({"id":"s1","query":"platypus aqueduct","top_k":1});
            let ranked = registry
                .get_host("web_fetch")
                .unwrap()
                .invoke(context(), query.clone())
                .await
                .unwrap();
            let ranked: Value = serde_json::from_str(&ranked.content).unwrap();
            assert_eq!(ranked["passages"][0]["number"], 7);
            let repeated = registry
                .get_host("web_fetch")
                .unwrap()
                .invoke(context(), query)
                .await
                .unwrap();
            let repeated: Value = serde_json::from_str(&repeated.content).unwrap();
            assert_eq!(repeated["passages"], ranked["passages"]);
            if let Some(original) = &original_passages {
                assert_eq!(&ranked["passages"], original);
            }
            original_passages = Some(ranked["passages"].clone());
            assert_eq!(searches.load(Ordering::Relaxed), 1);
            assert_eq!(fetches.load(Ordering::Relaxed), 1);
        }
        assert!(
            store
                .attachment(&first.id, "web-source:s1")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .attachment(&second.id, "web-source:s1")
                .unwrap()
                .is_some()
        );
        server.abort();
    }

    async fn counted_fixture() -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let fetches = Arc::new(AtomicUsize::new(0));
        let searches = Arc::new(AtomicUsize::new(0));
        let fetch_counter = fetches.clone();
        let search_counter = searches.clone();
        let app = Router::new()
            .route("/{page}", get(move || {
                let count = fetch_counter.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    ([("content-type", "text/plain")], "a🐾bcé")
                }
            }))
            .route("/search", get(move |axum::extract::Query(query): axum::extract::Query<HashMap<String,String>>| {
                let count = search_counter.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(query["q"], "cache fixture");
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    axum::Json(json!({"results":[
                        {"title":"One","url":"https://example.org/one","content":"One snippet"},
                        {"title":"Two","url":"https://example.org/two","content":"Two snippet"}
                    ]}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, fetches, searches, server)
    }

    #[tokio::test]
    async fn shared_fetch_cache_singleflight_pagination_misses_and_policy_partition() {
        let (base, fetches, _, server) = counted_fixture().await;
        let config = WebConfig {
            allow_private: true,
            ..Default::default()
        };
        let first = NetClient::new(&config).unwrap();
        let second = NetClient::new(&config).unwrap();
        let url = format!("{base}/one");
        let (a, b) = tokio::join!(first.fetch(&url, 0, 1), second.fetch(&url, 1, 2));
        assert_eq!(a.unwrap().content, "a");
        assert_eq!(b.unwrap().content, "🐾b");
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
        assert_eq!(
            second
                .fetch(&format!("{url}#fragment"), 3, 20)
                .await
                .unwrap()
                .content,
            "cé"
        );
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
        second.fetch(&format!("{base}/two"), 0, 1).await.unwrap();
        assert_eq!(fetches.load(Ordering::Relaxed), 2);
        let restricted = NetClient::new(&WebConfig::default()).unwrap();
        assert!(
            restricted
                .fetch(&url, 0, 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("prohibited address")
        );
        assert_eq!(fetches.load(Ordering::Relaxed), 2);
        let disabled = NetClient::new(&WebConfig {
            cache_ttl_secs: 0,
            ..config
        })
        .unwrap();
        disabled.fetch(&url, 0, 1).await.unwrap();
        disabled.fetch(&url, 0, 1).await.unwrap();
        assert_eq!(fetches.load(Ordering::Relaxed), 4);
        server.abort();
    }

    #[tokio::test]
    async fn shared_search_cache_normalizes_queries_and_partitions_backends() {
        let (base, _, searches, server) = counted_fixture().await;
        let config = WebConfig {
            search: vec![SearchBackend::SearxNg { url: base.clone() }],
            ..Default::default()
        };
        let first = NetClient::new(&config).unwrap();
        let second = NetClient::new(&config).unwrap();
        let (a, b) = tokio::join!(
            first.search(" CACHE  fixture ", 1),
            second.search("cache\nfixture", 2)
        );
        assert_eq!(a.unwrap().len(), 1);
        assert_eq!(b.unwrap().len(), 2);
        assert_eq!(searches.load(Ordering::Relaxed), 1);
        second.search("cache fixture", 2).await.unwrap();
        assert_eq!(searches.load(Ordering::Relaxed), 1);
        // Backend identity partitions entries even for the same normalized query.
        let (other, _, other_searches, other_server) = counted_fixture().await;
        let other_label = format!("searxng:{other}");
        let other = NetClient::new(&WebConfig {
            search: vec![SearchBackend::SearxNg { url: other }],
            ..config
        })
        .unwrap();
        let served = other.search_served("cache fixture", 2).await.unwrap();
        assert_eq!(served.results.len(), 2);
        assert_eq!(served.backend, other_label);
        assert_eq!(other_searches.load(Ordering::Relaxed), 1);
        other.search_served("cache fixture", 1).await.unwrap();
        assert_eq!(other_searches.load(Ordering::Relaxed), 1);
        server.abort();
        other_server.abort();
    }

    #[tokio::test]
    async fn cloned_handle_budgets_charge_only_singleflight_leader_and_misses() {
        let (base, fetches, searches, server) = counted_fixture().await;
        let client = NetClient::new(&WebConfig {
            allow_private: true,
            search: vec![SearchBackend::SearxNg { url: base.clone() }],
            max_fetches: Some(1),
            max_searches: Some(1),
            ..Default::default()
        })
        .unwrap();
        let clone = client.clone();
        let url = format!("{base}/one");
        let (a, b) = tokio::join!(client.fetch(&url, 0, 1), clone.fetch(&url, 0, 1));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
        let error = client
            .fetch(&format!("{base}/two"), 0, 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("fetch budget exhausted (limit 1)"));
        client.search("cache fixture", 1).await.unwrap();
        clone.search("cache fixture", 2).await.unwrap();
        assert_eq!(searches.load(Ordering::Relaxed), 1);
        assert!(
            clone
                .search("different query", 2)
                .await
                .unwrap_err()
                .to_string()
                .contains("search budget exhausted (limit 1)")
        );
        let fresh = NetClient::new(&client.config).unwrap();
        fresh.fetch(&url, 0, 1).await.unwrap();
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
        assert_eq!(client.budget.fetches.load(Ordering::Relaxed), 1);
        assert_eq!(client.budget.searches.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn permissive_search_cache_does_not_bypass_restricted_redirect_policy() {
        let app = Router::new()
            .route(
                "/search",
                get(|| async { axum::response::Redirect::temporary("/page") }),
            )
            .route(
                "/page",
                get(|| async {
                    axum::Json(json!({"results":[
                        {"title":"Fixture","url":"https://example.org/","content":"Fixture"}
                    ]}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WebConfig {
            search: vec![SearchBackend::SearxNg {
                url: format!("http://{}", listener.local_addr().unwrap()),
            }],
            allow_private: true,
            ..Default::default()
        };
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let allowed = NetClient::new(&config).unwrap();
        assert_eq!(
            allowed.search("redirect fixture", 1).await.unwrap().len(),
            1
        );
        let restricted = NetClient::new(&WebConfig {
            allow_private: false,
            ..config
        })
        .unwrap();
        assert!(
            restricted
                .search("redirect fixture", 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("prohibited address")
        );
        server.abort();
    }

    #[tokio::test]
    async fn ddg_and_brave_parsers_read_local_http_fixtures_without_real_key() {
        let app = Router::new()
            .route("/ddg",get(|| async { r#"<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone&amp;rut=ignored">One &amp; Two</a><a class="result__snippet">A <b>supporting</b> snippet.</a></div><div><a href='https://example.org/two' class='result__a'>Second</a><div class='result__snippet'>Second snippet</div></div>"# }))
            .route("/brave",get(|headers:axum::http::HeaderMap| async move {
                assert_eq!(headers.get("X-Subscription-Token").unwrap(),"synthetic-fixture-key");
                axum::Json(json!({"web":{"results":[{"title":"Brave fixture","url":"https://example.org/brave","description":"Fixture description"}]}}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = NetClient::new(&WebConfig {
            allow_private: true,
            ..Default::default()
        })
        .unwrap();
        let page = client
            .get(checked_url(&format!("{base}/ddg")).unwrap(), false, None)
            .await
            .unwrap();
        let results = parse_ddg(&String::from_utf8(page.body).unwrap(), 8).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://example.com/one");
        assert_eq!(results[0].title, "One & Two");
        assert_eq!(results[0].snippet, "A supporting snippet.");
        assert_eq!(results[1].url, "https://example.org/two");
        let page = client
            .get(
                checked_url(&format!("{base}/brave")).unwrap(),
                false,
                Some("synthetic-fixture-key"),
            )
            .await
            .unwrap();
        let results = parse_json_search(&page.body, true, 8).unwrap();
        assert_eq!(results[0].title, "Brave fixture");
        assert_eq!(results[0].snippet, "Fixture description");
        server.abort();
    }

    /// Per request: (Content-Type, Authorization, JSON body).
    type CrawlLog = Arc<std::sync::Mutex<Vec<(Option<String>, Option<String>, Value)>>>;

    /// Local stand-in for Crawl4AI's `POST /crawl`. It never fetches the target itself; it
    /// reports a redirect's final URL the way the real server does.
    async fn crawl4ai_fixture() -> (String, CrawlLog, tokio::task::JoinHandle<()>) {
        use axum::response::IntoResponse;
        let log: CrawlLog = Arc::default();
        let recorded = log.clone();
        let app = Router::new().route(
            "/crawl",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let recorded = recorded.clone();
                    async move {
                        let content_type = headers
                            .get(header::CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        let authorization = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        let target = body["urls"][0].as_str().unwrap_or_default().to_owned();
                        let authorized = authorization.as_deref() == Some("Bearer synthetic-token");
                        recorded
                            .lock()
                            .unwrap()
                            .push((content_type, authorization, body));
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        if target.ends_with("/auth-required") && !authorized {
                            return (
                                axum::http::StatusCode::UNAUTHORIZED,
                                axum::Json(json!({"detail": "Not authenticated"})),
                            )
                                .into_response();
                        }
                        if target.ends_with("/error") {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(json!({"detail": "boom"})),
                            )
                                .into_response();
                        }
                        if target.ends_with("/failed") {
                            return axum::Json(json!({"success": false, "results": []}))
                                .into_response();
                        }
                        if target.ends_with("/blocked") {
                            return axum::Json(json!({"success": true, "results": [{
                                "url": target, "redirected_url": target, "success": false,
                                "error_message": "blocked by anti-bot protection",
                                "markdown": null
                            }]}))
                            .into_response();
                        }
                        // The crawl followed a redirect to an internal address. The content
                        // must be dropped, whether or not the crawl reports success.
                        if target.ends_with("/redirect-private") {
                            return axum::Json(json!({"success": true, "results": [{
                                "url": target, "redirected_url": "http://192.168.1.1/admin",
                                "success": true,
                                "markdown": {"raw_markdown": "internal secret",
                                             "fit_markdown": "internal secret"}
                            }]}))
                            .into_response();
                        }
                        if target.ends_with("/redirect-private-failed") {
                            return axum::Json(json!({"success": true, "results": [{
                                "url": target, "redirected_url": "http://10.0.0.5/",
                                "success": false, "error_message": "anti-bot",
                                "markdown": {"raw_markdown": "internal secret",
                                             "fit_markdown": "internal secret"}
                            }]}))
                            .into_response();
                        }
                        if target.ends_with("/redirect-public") {
                            return axum::Json(json!({"success": true, "results": [{
                                "url": target, "redirected_url": "http://1.1.1.1/landing",
                                "success": true,
                                "markdown": {"raw_markdown": "public landing",
                                             "fit_markdown": "public landing"}
                            }]}))
                            .into_response();
                        }
                        axum::Json(json!({"success": true, "results": [{
                            "url": target, "redirected_url": target, "success": true,
                            "markdown": {"raw_markdown": "# Title\n\nHello 🐾 world",
                                         "fit_markdown": "# Title\n\nHello 🐾 world"}
                        }]}))
                        .into_response()
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, log, server)
    }

    #[tokio::test]
    async fn crawl4ai_fetch_posts_markdown_caches_and_partitions_by_service() {
        let (service, recorded, server) = crawl4ai_fixture().await;
        let config = WebConfig {
            allow_private: true,
            fetch: vec![FetchBackend::Crawl4Ai {
                url: service.clone(),
            }],
            ..Default::default()
        };
        let target = "https://example.org/article?page=1#section";
        let first = NetClient::new(&config).unwrap();
        let page = first.fetch(target, 0, 100).await.unwrap();
        assert_eq!(page.url, "https://example.org/article?page=1");
        assert_eq!(page.title, "");
        assert_eq!(page.content, "# Title\n\nHello 🐾 world");
        assert_eq!(page.total_chars, page.content.chars().count());
        assert_eq!(page.next_offset, None);
        {
            let log = recorded.lock().unwrap();
            assert_eq!(log.len(), 1);
            assert_eq!(log[0].0.as_deref(), Some("application/json"));
            assert_eq!(
                log[0].2["urls"],
                json!(["https://example.org/article?page=1"])
            );
            assert_eq!(
                log[0].2["crawler_config"]["params"]["markdown_generator"]["params"]["content_filter"]
                    ["type"],
                "PruningContentFilter"
            );
        }
        // A new handle with the same configuration hits the shared cache.
        let second = NetClient::new(&config).unwrap();
        assert_eq!(second.fetch(target, 0, 5).await.unwrap().content, "# Tit");
        assert_eq!(recorded.lock().unwrap().len(), 1);
        // Another Crawl4AI service never shares entries with this one.
        let (other, other_recorded, other_server) = crawl4ai_fixture().await;
        let other_client = NetClient::new(&WebConfig {
            fetch: vec![FetchBackend::Crawl4Ai { url: other }],
            ..config.clone()
        })
        .unwrap();
        other_client.fetch(target, 0, 5).await.unwrap();
        assert_eq!(other_recorded.lock().unwrap().len(), 1);
        assert_eq!(recorded.lock().unwrap().len(), 1);
        // Cached pagination spends no fetch budget.
        let budgeted = NetClient::new(&WebConfig {
            max_fetches: Some(1),
            ..config.clone()
        })
        .unwrap();
        budgeted.fetch(target, 0, 5).await.unwrap();
        budgeted.fetch(target, 0, 5).await.unwrap();
        assert_eq!(budgeted.budget.fetches.load(Ordering::Relaxed), 0);
        server.abort();
        other_server.abort();
    }

    #[tokio::test]
    async fn crawl4ai_errors_name_the_backend_and_never_fall_back_or_cache() {
        let (service, recorded, server) = crawl4ai_fixture().await;
        let client = NetClient::new(&WebConfig {
            allow_private: true,
            fetch: vec![FetchBackend::Crawl4Ai { url: service }],
            ..Default::default()
        })
        .unwrap();
        for (target, expected) in [
            (
                "https://example.org/error",
                "crawl4ai fetch backend returned HTTP 500",
            ),
            (
                "https://example.org/failed",
                "crawl4ai fetch backend reported a failed crawl",
            ),
            (
                "https://example.org/blocked",
                "crawl4ai fetch backend could not crawl the page: blocked by anti-bot protection",
            ),
        ] {
            let error = client.fetch(target, 0, 5).await.unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
        // Failures are not cached, so a retry reaches the service again.
        assert!(
            client
                .fetch("https://example.org/error", 0, 5)
                .await
                .is_err()
        );
        // Three distinct failures plus the retry: four requests reached the service.
        assert_eq!(recorded.lock().unwrap().len(), 4);
        server.abort();
        // A stopped service fails explicitly; the harness does not fetch the page itself.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let down = NetClient::new(&WebConfig {
            allow_private: true,
            fetch: vec![FetchBackend::Crawl4Ai {
                url: format!("http://127.0.0.1:{port}"),
            }],
            ..Default::default()
        })
        .unwrap();
        let error = down
            .fetch("https://example.org/article", 0, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.ends_with(": crawl4ai fetch backend is unreachable"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn crawl4ai_checks_target_addresses_but_exempts_the_service() {
        let (service, recorded, server) = crawl4ai_fixture().await;
        let client = NetClient::new(&WebConfig {
            fetch: vec![FetchBackend::Crawl4Ai { url: service }],
            ..Default::default()
        })
        .unwrap();
        // Loopback targets are refused before anything is sent to the service.
        let error = client
            .fetch("http://127.0.0.1:9/private", 0, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("prohibited address"), "{error}");
        assert!(recorded.lock().unwrap().is_empty());
        // A loopback service is allowed without allow_private; a public IP target goes through.
        let page = client.fetch("http://8.8.8.8/doc", 0, 5).await.unwrap();
        assert_eq!(page.content, "# Tit");
        assert_eq!(recorded.lock().unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn crawl4ai_drops_content_from_redirects_to_internal_addresses() {
        let (service, recorded, server) = crawl4ai_fixture().await;
        let client = NetClient::new(&WebConfig {
            fetch: vec![FetchBackend::Crawl4Ai {
                url: service.clone(),
            }],
            ..Default::default()
        })
        .unwrap();
        // An internal final URL is refused whether or not the crawl reports success, and the
        // page content appears neither in the error nor in a page.
        for target in [
            "http://8.8.8.8/redirect-private",
            "http://8.8.8.8/redirect-private-failed",
        ] {
            let error = client.fetch(target, 0, 100).await.unwrap_err().to_string();
            assert!(
                error.ends_with(
                    ": crawl4ai fetch backend followed a redirect to a prohibited address"
                ),
                "{error}"
            );
            assert!(!error.contains("internal secret"));
        }
        // Refusals are errors, so they are not cached: the same URL is crawled again.
        assert!(
            client
                .fetch("http://8.8.8.8/redirect-private", 0, 100)
                .await
                .is_err()
        );
        assert_eq!(recorded.lock().unwrap().len(), 3);
        // A public final URL is returned, reported as the page URL.
        let page = client
            .fetch("http://8.8.8.8/redirect-public", 0, 100)
            .await
            .unwrap();
        assert_eq!(page.url, "http://1.1.1.1/landing");
        assert_eq!(page.content, "public landing");
        // allow_private permits internal final URLs, as it permits direct internal fetches.
        let permissive = NetClient::new(&WebConfig {
            allow_private: true,
            fetch: vec![FetchBackend::Crawl4Ai { url: service }],
            ..Default::default()
        })
        .unwrap();
        let page = permissive
            .fetch("http://8.8.8.8/redirect-private", 0, 100)
            .await
            .unwrap();
        assert_eq!(page.url, "http://192.168.1.1/admin");
        assert_eq!(page.content, "internal secret");
        server.abort();
    }

    #[tokio::test]
    async fn crawl4ai_sends_bearer_token_only_when_supplied() {
        let (service, recorded, server) = crawl4ai_fixture().await;
        let client = NetClient::new(&WebConfig {
            allow_private: true,
            fetch: vec![FetchBackend::Crawl4Ai {
                url: service.clone(),
            }],
            ..Default::default()
        })
        .unwrap();
        let target = checked_url("https://example.org/auth-required").unwrap();
        // Without a token the authenticated service answers 401, reported with the backend name.
        let error = client
            .crawl4ai_page(&service, &target, None)
            .await
            .err()
            .expect("the unauthenticated request must fail")
            .to_string();
        assert_eq!(error, "crawl4ai fetch backend returned HTTP 401");
        // With a token the header is sent and the page comes back.
        let page = client
            .crawl4ai_page(&service, &target, Some("synthetic-token"))
            .await
            .unwrap();
        assert_eq!(page.body, "# Title\n\nHello 🐾 world".as_bytes());
        {
            let log = recorded.lock().unwrap();
            assert_eq!(log.len(), 2);
            assert_eq!(log[0].1, None);
            assert_eq!(log[1].1.as_deref(), Some("Bearer synthetic-token"));
        }
        assert!(non_blank_token(Some("   ".into())).is_none());
        assert_eq!(
            non_blank_token(Some("synthetic-token".into())).as_deref(),
            Some("synthetic-token")
        );
        server.abort();
    }
}
