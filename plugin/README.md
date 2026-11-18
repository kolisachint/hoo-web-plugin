# webtools plugin for hoocode

Web fetch and search for [hoocode](https://github.com/kolisachint/hoocode), as an
Agent Plugins 1.0 package. The fetch and search code is the `webtools` libraries
(`crates/core`, `crates/webfetch`, `crates/websearch`), built into a WASI 0.2
component that hoocode runs in its sandbox. Every network request goes through
hoocode's permission gate and its SSRF block.

Pages come back as token-efficient text: links become inline `[N]` markers, and
the full URLs sit in a references list at the end.

## Tools

hoocode exposes them as `ext_webtools_fetch` and `ext_webtools_search`.

| Tool | Arguments | Result |
|---|---|---|
| `fetch` | `url` (required); `output` (`text` \| `markdown` \| `structured`, default `text`); `max_tokens` (default 6000); `offset`; `outline`; `grep`; `timeout` (seconds, default 10); `json` | Compact: `{url, final_url, title?, content, truncated, next_offset?}`. `json: true`: the whole `FetchResult`. |
| `search` | `query` (required); `max_results` (default 5); `safe_search` (`on` \| `off`); `timeout` (default 10); `json` | Compact: `{query, provider, result_count, content}`. `json: true`: the whole `SearchOutput`. |

Argument names and defaults match the webtools MCP server (`src/mcp.rs`). Invalid
arguments return `invalid-args`. A page with no extractable text, or a blocked
search, returns a failure with the reason in the message.

The skill `skills/web` tells the model when to use each tool and how to keep
budgets small.

## Settings

The host reads these through `config-get` from the user's hoocode settings. The
plugin ships no settings of its own.

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
