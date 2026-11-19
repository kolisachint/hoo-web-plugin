# webtools plugin for hoocode

Web fetch and search for [hoocode](https://github.com/kolisachint/hoocode), as an
Agent Plugins 1.0 package. The fetch and search code is the `webtools` libraries
(`crates/core`, `crates/webfetch`, `crates/websearch`), built into a WASI 0.2
component that hoocode runs in its sandbox. Every network request goes through
hoocode's permission gate and its SSRF block.

Pages come back as token-efficient text: links become inline `[N]` markers, and
the full URLs sit in a references list at the end.

## Tools

hoocode exposes them as `WebFetch` and `WebSearch`, the names the component declares,
with no prefix. The plugin's origin is kept apart from the name, so permission rules
can select a tool in two ways: by tool name (`WebFetch`, `WebSearch`) or by origin
(`plugin:webtools`, every tool from this plugin).

| Tool | Arguments | Result |
|---|---|---|
| `WebFetch` | `url` (required); `output` (`text` \| `markdown` \| `structured`, default `text`); `max_tokens` (default 6000, at least 1); `offset`; `outline`; `grep`; `timeout` (seconds, default 10, capped at 8); `json` | Compact: `{url, final_url, title?, content, truncated, next_offset?}`. `json: true`: the whole `FetchResult`. |
| `WebSearch` | `query` (required, not empty); `max_results` (default 5); `safe_search` (`on` \| `off`); `timeout` (seconds, default 12, capped at 12); `json` | Compact: `{query, provider, result_count, content}`. `json: true`: the whole `SearchOutput`. |

Argument names and defaults match the webtools MCP server (`src/mcp.rs`), which keeps
the lowercase names `fetch` and `search`, except the search timeout: 12 seconds here (capped at 12), 10 in the MCP server. Invalid
arguments return `invalid-args`. A `max_tokens` of 0, a `grep` pattern that does not
compile, an empty or blank `query`, and an unknown `output` or `safe_search` are
refused before any request is made. An `offset` at or past the end of the page is
only known once the page is fetched, so it is refused after the fetch, and the
message gives the page size. hoocode traps a tool call at 30 seconds, so the
component runs each call under a 25-second budget: the whole fetch (redirects and
retries included) or the whole search (fallback provider included) stops at that
point.

A search with a fallback configured gives the primary at most 16 seconds. That
keeps the last 9 seconds of the call for the fallback: 8 seconds is the minimum it
needs to start, and the extra second is slack for a primary that returns a little
after its deadline. A slow or timing-out primary therefore cannot starve the
fallback, at the cost of a fallback that starts later and gets less time. The
fallback is tried if at least 8 seconds are left once the primary has returned.
A page with no extractable text, or a blocked search, returns a failure with the
reason in the message.

The skill `skills/web` tells the model when to use each tool and how to keep
budgets small.

## Settings

The host reads these through `config-get` from hoocode's settings, under
`pluginConfig.webtools.<key>`. The plugin ships no settings of its own. For
example, in hoocode's settings file:

```json
{"pluginConfig": {"webtools": {"searchProvider": "brave", "braveApiKey": "..."}}}
```

| Key | Meaning | Default |
|---|---|---|
| `searchProvider` | `duckduckgo`, `brave`, `tavily` or `searxng` | `duckduckgo` (no key) |
| `searchFallback` | Provider tried when the primary fails or is blocked. `none` turns it off. | none |
| `braveApiKey` | Brave Search API key. Needed for `brave`. | |
| `tavilyApiKey` | Tavily API key. Needed for `tavily`. | |
| `searxngUrl` | Base URL of a SearXNG instance. Needed for `searxng`. | |
| `searxngApiKey` | Optional bearer key for SearXNG. | |

A keyed provider with no key is a failure, not a silent fallback to DuckDuckGo.

## Layout

```
plugin.json              Agent Plugins 1.0 manifest
skills/web/SKILL.md      when to fetch, when to search, budgets, citing
dev.hoocode/extension.wasm   the component (built, committed)
component/               the guest crate (own Cargo.lock, not a workspace member)
  wit/extension.wit      copy of hoocode's hoocode:extension@0.1.0 contract
  src/lib.rs             the two tools
build.sh                 builds the component into dev.hoocode/extension.wasm
```

## Build

Needs Rust with the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`).

```bash
plugin/build.sh
```

The script builds `plugin/component` in release mode (`opt-level = "s"`, LTO,
stripped) and copies the result to `dev.hoocode/extension.wasm`. The build is
byte-reproducible from any checkout path and any target directory:

- The sources are copied to a fixed staging path (`/tmp/webtools-plugin-src`)
  and built there. Cargo hashes the absolute path of the path dependencies in
  `crates/` into the crate metadata, which changes the code layout, so the path
  must be the same for every clone. Do not make the staging path configurable.
- Paths in the output are remapped: the staging tree, the target directory, the
  cargo home and the rustup sysroot.
- The build uses `--locked`.
- The staging path is shared by every user of the host, so the script takes an
  `flock` on `/tmp/webtools-plugin-src.lock` where `flock` exists, creates the
  stage with `mkdir` (no `-p`) so it fails rather than reusing a directory, and
  checks that the stage is owned by you and not group- or world-writable. Without
  `flock`, concurrent builds on one host are unsupported.

The output depends on the rustc version. CI pins `1.97.0`, so rebuild with that
version: `rustup toolchain install 1.97.0 -t wasm32-wasip2` and
`RUSTUP_TOOLCHAIN=1.97.0 plugin/build.sh`. CI runs `git diff --exit-code` on the
artifact, so a rebuild with another compiler changes the committed file. Set
`WEBTOOLS_TARGET_DIR` to choose the target directory.

The WIT file in `component/wit/` is a copy of hoocode's `wit/extension.wit`. It
must match the hoocode release the plugin targets. Switch to the git tag
`wit-v0.1.0` when it exists.

## Install

Copy or clone this folder into `~/.agents/plugins/webtools/`, or into a project's
`.agents/plugins/webtools/`. The `plugin/` folder is the plugin root.

## License

MIT, as the rest of the webtools workspace.
