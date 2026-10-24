# Web research and tools

Status: implemented, with DeepSeek search as the default 2026-10-18. Search transport
and fallback acceptance use local HTTP fixtures. After an initial HTTP 402
`insufficient_balance_error`, a funded retry on 2026-10-18 through the actual
BubblePaw `NetClient::search_served` returned three public Rust documentation
results. Cache and fallback were disabled for that retry; its served backend was
`deepseek`. This qualifies one live search and normalization, not broad research
quality. Sanitized evidence is `deepseek-search-live-20261018-retry.json` in the
usage-analysis workspace; the original failed probe remains preserved.

Web capabilities belong to the runner library's `net` module, independently of
the optional web workbench feature. `--web` or `web.enabled=true` enables
`web_search`, `web_fetch` and the ordinary `research` child profile. The main agent
uses the same tools directly for quick lookups. The child decides how many
searches and fetches its task needs.

## Research child

Research uses the [normal child-session path](child-sessions.md):

```js
const child = await tools.agent({profile: "research", task: "Investigate the question and cite source IDs."});
state.researchSession = child.sessionId;
return await wait({sessionId: child.sessionId});
```

Follow up in the same saved child:

```js
await tools.agent_send({sessionId: state.researchSession, text: "Which source supports that date?"});
return await wait({sessionId: state.researchSession});
```

The child is a visible, openable session with canonical history and ordinary
status, wait, send and cancel controls. It receives the parent's effective
approval policy, web tools and workspace file reads, with JS as the model entry
point. It has no command, write or child-spawning capabilities. Its model inherits
the parent's active model and its round limit uses the host Harness configuration.
Optional `[agents]` limits apply normally. It answers with source IDs and numbered
passages, such as `s3#p12`, rather than retyping URLs.

Research accepts the same optional `output_schema`, `max_tool_calls` and `timeout`
as every [child profile](child-sessions.md#structured-output-tool-budgets-and-timeout).
There are no research-specific defaults for these controls; ended children remain
continuable through ordinary follow-ups.

There is no `research` tool, quick/thorough mode, `[web.research]`,
`web.expose.research` or `--research-model` flag. The inspected fact-checker
consumer uses `net::register_research_sources` with `parallel::ToolLoop`, not the
removed research tool; no deprecated tool shim is retained. That separate loop
is outside this change.

## Configuration

```toml
[web]
enabled = true
search = ["deepseek", "ddg"]
fetch_backend = ["direct"]
key_env = "BRAVE_API_KEY"     # Environment-variable name, never a credential.
deepseek_key_env = "DEEPSEEK_API_KEY"
deepseek_base_url = "https://api.deepseek.com/v1/tools"
cache_ttl_secs = 86400        # 24 hours; 0 disables caching.
cache_max_bytes = 67108864    # 64 MiB; see physical storage limits below.
cooldown_secs = 300
allow_private = false
user_agent = "BubblePaw/0.1"
# max_searches = 100          # Optional per-client miss budgets; unset is unlimited.
# max_fetches = 200

[web.expose]
fetch = true
search = true
```

DeepSeek is the default first search backend, followed by DuckDuckGo. Web tools
remain disabled until enabled. To make SearXNG first, put `searxng:<base-url>`
first in `web.search`. `--web-search searxng:<url>` sets a single backend;
`--set 'web.search=["searxng:<url>","ddg"]'` sets an ordered list.
`--web-expose fetch,search` selects the parent tools; both default to exposed.
Research always receives both. Normal TOML, environment and CLI layering applies;
see [configuration](config.md). DeepSeek and Brave read their named environment
variables only when enabled and configured. Missing keys skip their respective
backends with a warning, allowing the rest of the chain to run. Config
printing never reads the key; credentials are not accepted in config files or
CLI flags and are not logged.

### Live backend configuration (2026-10-14)

`web.search`, `web.fetch_backend`, `web.key_env`, `web.deepseek_key_env`,
`web.deepseek_base_url` and `web.cooldown_secs` apply
with provider reload: `POST /api/config/reload` in a web host or stdio
`config.reload`. The host prepares a replacement before applying it, and resolves
credential environment variables at apply time. An in-flight search/fetch keeps
its old backend snapshot; subsequent calls from existing sessions, children and
workers use the new backend configuration. Failed preparation preserves active
settings. No file watcher is installed.

`web.enabled`, exposure, private-address/security rules, user agent, cache settings
and budgets remain selected at startup and require a restart. Existing source
records, chat history and disposable cache boundaries are unchanged. See
[configuration](config.md#live-provider-configuration-2026-10-14).

## Tools and citations

`web_search({query, count?: 8})` returns the serving backend and up to 20 results.
Each result carries its `id`, `url`, `title` and `snippet`, plus `published_at` when
DeepSeek supplies a publication date. Search results are leads;
fetch supporting pages before citing their content.

`web_fetch({url?, id?, offset?: 0, max_chars?: 20000, query?, top_k?: 5})`
accepts exactly one of `url` and `id`. Its result identifies the source and backend,
provides readable Markdown/text, and includes numbered passages. Without `query`,
character offsets and `next_offset` page through text; offsets count Unicode
characters. With `query`, BM25 in Rust selects the top passages, without a model
call or embeddings. Passage numbers refer to the complete cached page, not their
rank in the returned subset.

Passages split on Markdown structure boundaries, with long blocks divided into
chunks of a few hundred words. HTML uses the existing pinned `html2text`
conversion; text media types and JSON remain text. Unsupported media types fail.
The passage split is stored with readable text, so reads of a cached page retain
numbering. Retrieval after expiry can contain different page text and numbering.

Source IDs are scoped to one root chat and shared with all its descendants.
New root chats have independent sequences starting at `s1`. The URL/title mapping
is persisted as immutable source attachments in the root's authoritative chat
record, separate from the response cache. Repeated results reuse the same ID for
a normalized URL, including after store reopen. Deleting the shared cache does not
lose saved mappings or history. `web_fetch({id:"s1"})` resolves a saved citation;
the returned `source` object maps its ID to the normalized requested URL and
first-observed title/backend. The top-level `url` names the final fetched URL
after redirects. These immutable mappings retain their first-observed metadata.

Native replies fit within 60 KiB, including JSON escaping and metadata. Direct
requests have a 20-second deadline, 5 MB body cap and at most five redirects.
Crawl4AI requests have a 60-second deadline and the same body cap.
Both web calls are operation waits in CodeMode, so its non-wait idle deadline
does not shorten a network request or fallback chain. Cancellation still stops
the request.

## Backend fallback

Search and fetch check ready entries from configured backends in order before
making a network request. A cached configured fallback can answer while an earlier
backend has no ready entry; unconfigured backends never serve cached results.
On a cache miss, requests try the configured backends in order. Errors, timeouts, 429,
5xx, block/challenge pages and empty search results move to the next backend.
A wholly empty search returns an empty result; an all-error chain reports each
backend failure. Successful outputs name the backend that served them.

Rate-limited or blocked search backends and rate-limited Crawl4AI services enter
a process-wide cooldown using `Retry-After`, capped at 24 hours, or
`web.cooldown_secs`. A target site's 429 or challenge affects that page only.
Cached responses remain reusable without another network call.

DeepSeek uses `POST <deepseek_base_url>/search` with bearer authentication and
`{query, count: 20, need_content: false}`. The fixed retrieval count preserves the
existing shared cache across requested result counts. Its title, URL, snippet and
nullable date map into normal search results and source IDs. Redirects are rejected
before credentials can reach another endpoint; errors omit response bodies and keys.
This is the direct Search API, with no auxiliary model call. The contract follows
the [official DeepSeek API provider](https://github.com/deepseek-ai/deepseek-harness/tree/master/packages/web/web-deepseek-api).

Fetch backends are `direct` and `crawl4ai:<base-url>`. Crawl4AI receives
`POST <base>/crawl` with the target and pruning configuration; the runner reads
`markdown.fit_markdown` and the reported final URL. It sends `CRAWL4AI_API_TOKEN`
as a bearer header when set, without logging it. Crawl4AI's own cache is bypassed.
The harness uses direct GET only when `direct` is listed in the chain.

## Shared cache and budgets

Store-aware clients use `<data_dir>/web-cache.db`, next to `catalog.db`, never
inside catalog or chat databases. This disposable SQLite file stores successful
search replies and converted page text with its passage split. It contains no raw
HTML and no session source IDs. Independent sessions and host processes reuse it;
deleting it requires only retrieving pages again.
Close the host/cache owners before removing the SQLite file and its sidecars.

Fetch keys include normalized requested URL and backend; search keys include
normalized query and backend. Private-address permission and user agent partition
both; DeepSeek and Brave credentials appear only as digests in backend identity.
DeepSeek base URLs also partition entries. Queries are
trimmed, whitespace-collapsed and lowercased. Search retrieves up to 20 results
so different requested counts reuse the entry. Direct and Crawl4AI caches remain
separate, including different Crawl4AI servers. Concurrent same-key misses share
one request in a host process; cancellation lets a waiter retry.

TTL is measured from completed retrieval and defaults to 24 hours. Zero bypasses
the cache. `cache_max_bytes` bounds serialized responses plus their keys through
LRU eviction; SQLite schema/index pages and page rounding add physical overhead.
Access timestamps are touched at most once per minute to keep pagination cheap.
Eviction drains incremental vacuum and truncates WAL; normal writes use WAL
autocheckpoint at 128 pages and a 1 MiB journal retention limit. That retention
limit does not cap every transient WAL peak. See acceptance measurements below.

`max_searches` and `max_fetches` are optional, unlimited by default. Positive
values enable per-client budgets; clones share counters. Cache hits, pagination
of cached pages and single-flight waiters do not consume slots. A call needing
network access spends one slot even if its chain tries several backends or fails.
Exhaustion is an explicit tool error.

## Address safety

Only HTTP(S) URLs without embedded credentials are accepted. Default checks
refuse loopback, private, link-local, unique-local and metadata addresses. DNS
answers are checked and approved addresses pinned for direct requests; every
redirect destination is checked again. Environment HTTP proxies are disabled.
`allow_private=true` explicitly permits internal targets.

Configured SearXNG and Crawl4AI service URLs are operator configuration, exempt
from target checks; that does not authorize arbitrary private fetches. Crawl4AI
target and reported final URL are checked before using content. Intermediate
redirects happen inside Crawl4AI and the harness cannot inspect them. Run that
service with no route to internal networks to prevent intermediate private requests.

## Embedding

Register parent tools with
`net::register_fetch_search_tools_with_store(&mut tools, &web, &store)` before
building child profiles. `profiles::child_agent_host` adds research when passed
enabled web configuration. For custom hosts,
`research::research_profile_with_tools` accepts the host registry and restricts it
to file reads plus web tools. Register ordinary controls and shared wait/cancel
through `AgentTools` and `agent_handles`.

`NetClient` supports `with_store` and explicit `with_cache_path`. Legacy
fetch/search registration and `NetClient::new` remain available for embedders,
using a shared process-local in-memory SQLite cache without durable source IDs.
Use store-aware registration for durable chat IDs and the shared disk cache;
an explicit cache path supplies persistence for a lower-level client.
Networking, source allocation and orchestration stay outside `bubblepaw-core`.

## Offline verification (2026-10-13)

The 20 focused web-tool unit tests pass on Linux with Rust 1.97.1. They cover
root/child source allocation and physical Store reopen, stable passage numbers,
BM25 ranking of a query in passage 7, policy/backend cache partitioning,
single-flight miss budgets, persistence, TTL expiry, eviction and cache deletion.
The net (4), backend fallback (10) and research-child (8) integration tests also
pass. Research acceptance drives main/child model decisions, file reads, web
retrieval, cited answers, follow-up in the same child, direct parent retrieval,
and fresh-store source lookup. Two independent roots reuse one search/fetch
with separate source scopes; cached BM25 retrieval keeps passage 7's number.
All seven focused CodeMode wait tests pass, including web replies held for
150 ms against a 30 ms idle limit and cancellation with observed connection close.
Fixtures use local HTTP servers and scripted providers; no live/paid model or
real web requests are part of acceptance. Private transcript-fixture coverage is
skipped because `BUBBLEPAW_FIXTURES_DIR` is unset.

All commands run on Linux with Rust 1.97.1:

- `cargo +1.97.1 fmt --all --check` passes on the final Rust files.
- Strict Clippy passes for the workspace/all targets, both default and `--features web`.
- `cargo +1.97.1 test --workspace --no-fail-fast` runs once. It fails two newly
  added assertions: a source label expects `custom` instead of `custom: <path>`,
  and an invalid web exposure expects its value echoed instead of the allowed
  methods diagnostic. All other groups pass. Both assertions are corrected
  without production changes; the affected runner library (129 tests) and web
  CLI target (4 tests) pass on rerun. There is no second full workspace run.

One worker formats its test file with standalone Windows rustfmt before the
Linux-only workflow is corrected. No build or test runs on Windows; the final
formatting and acceptance checks run on Linux.

The local measurement fetches 50 distinct HTML URLs through store-aware tools,
then reads 50 cached pages. These are synthetic repeated paragraphs, not a
representative real-site corpus. WAL is sampled after each completed fetch,
so the sampled maximum can miss peaks within a write.
Transaction counts cover application cache/source writes, excluding schema
initialization and the Harness's ordinary tool-message/effect commits.

| Measurement | Bytes / count |
|---|---:|
| Serialized responses plus cache keys | 1,097,280 bytes |
| Cache database before / after idle checkpoint | 1,155,072 / 1,155,072 bytes |
| Largest sampled cache WAL | 564,472 bytes |
| Cache WAL after idle checkpoint | 0 bytes |
| Cache application write transactions / maintenance writes | 50 / 0 |
| Authoritative source-mapping chat transactions | 50 |
| Additional catalog / serving transactions | 0 / 0 |
| Additional HTTP / cache / chat writes for 50 cached page reads | 0 / 0 / 0 |

After closing all Store/cache owners, the fixture deletes its cache file and
reopens the Store. Fetching saved `s7` resolves the original `/p6` mapping and
makes one HTTP request to refill the cache. A separate small-cache fixture makes
50 evicting writes under a 64 KiB payload/key bound, checks sampled DB+WAL stays
within 256 KiB, and checks checkpointed database size stays within 96 KiB.
