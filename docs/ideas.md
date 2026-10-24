# Ideas

Status: parked ideas, not on the roadmap.

Parked ideas. Not on the roadmap until someone picks them up.

## Native web research (built, 2026-10-09)

Give any BubblePaw agent web access through a small surface: `research {question, depth}` runs a research subagent (search, read 2-5 sources, cross-check, answer with sources), and `web_fetch {url}` reads a known page. Raw `web_search` is opt-in. Search backends: SearXNG, Brave, DuckDuckGo. Fetches to private addresses are refused by default.

This is built on standardized **agent profiles** (named child harnesses with their own tools, prompt, model and round limit), which replace the hard-coded read-only and workspace children.

Integrated from `web-research` at `2d80999` into `host-support`, with a shared process cache and per-handle search/fetch budgets. See [web.md](web.md) for configuration. Web tools remain opt-in. Live search quality is unverified.
