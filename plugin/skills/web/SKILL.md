---
name: web
description: Read a web page or search the web with the webtools tools (ext_webtools_fetch, ext_webtools_search). Use fetch when the user gives a URL or a page must be read; use search when the answer needs sources the user did not name. Covers token budgets and citing sources.
---

# Web: fetch and search

Two tools, both from the `webtools` plugin.

## Choose the tool

- **ext_webtools_fetch** reads one URL. Use it when the user gives a link, or when
  a search result must be checked before it is quoted.
- **ext_webtools_search** finds pages for a question. Use it when the sources are
  not known. Search results are titles and snippets only: fetch a page before you
  state a detail that depends on it.

## Budgets

- `fetch`: `max_tokens` defaults to 6000. Keep the default for a normal page.
  For a long page, call `outline: true` first (headings and the `offset` of each
  section), then fetch the one section you need with that `offset`.
- A budgeted fetch that stopped early returns `truncated: true` and a
  `next_offset`. Pass it back as `offset` to read on. Do not refetch from zero.
- `grep` returns where a pattern appears, with the surrounding text. Use it to
  find one mention on a long page instead of reading the whole page.
- `search`: `max_results` defaults to 5. Ask for fewer when one good source is
  enough. Refine the query rather than raising the count.
- Do not fetch every search hit. Fetch the two or three that look most likely.

## Failures

- A page with no extractable text (`status` is `empty` or `needs_js`) did not
  answer. Say so, and try another source. Do not treat it as "nothing there".
- A search reported as blocked did not answer the question. It is not the same as
  no results. Say that, and retry once with a different query or provider.

## Citing

- Cite the `final_url` (or the `url` you asked for) of each page you used.
- Keep the `[N]` markers in quoted text tied to the references list the tool
  returned, so a reader can follow each marker to its URL.
- Attribute facts to the page they came from. Mark anything taken only from a
  search snippet as unverified.
