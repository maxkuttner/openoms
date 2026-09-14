# Move the cockpit docs onto GitHub Pages

**Date:** 2026-09-14
**Status:** approved design, not yet implemented
**Follows:** `2026-09-14-install-landing-page-design.md`, which put the landing page at
`maxkuttner.github.io/openoms` and gave the `oms` binary an embedded cockpit.

## Problem

Two of the cockpit's pages are documentation, not operation:

- `cockpit/src/pages/Architecture.tsx` (389 lines) — the instrument data model, as
  prose plus four mermaid diagrams.
- `cockpit/src/pages/ApiDocs.tsx` (326 lines) — an API reference rendered from the
  OMS's own OpenAPI spec, fetched at runtime from `/api-docs/openapi.json`.

Both are behind the cockpit, which means behind an installed binary, a provisioned
database and a login. Someone deciding whether to try openoms cannot read either.
That is exactly backwards: the architecture page and the API reference are what a
prospective user wants *before* installing, and they are the least operational
things in the app.

They also cost the shipped binary. `mermaid` is roughly a megabyte of JavaScript
carried inside every release, for a page no operator opens twice.

## What we are building

Both pages become static HTML on the Pages site, linked from the landing page, and
are deleted from the cockpit.

The constraint carried over from the landing page spec: **the published pages are
hand-written HTML with no framework, no CDN and no external requests.** A page that
tells people to pipe a script into their shell has no business loading third-party
JavaScript, and that reasoning does not weaken for the pages next to it.

## Out of scope

- **`/scalar` and `/api-docs/openapi.json` stay on the OMS.** They are API endpoints,
  not cockpit UI; the Python client and anyone scripting against a running server may
  depend on them. Removing them is a separate decision with separate consumers.
- **The cockpit itself stays.** Only its two documentation pages move.
- **`docs/tutorials/` stays where it is.** Publishing it is a later question.

## 1. Getting the spec out of the binary

The API reference needs the OpenAPI spec at *build* time. Today the only way to get
it is to run the server and GET `/api-docs/openapi.json`, which a static site build
cannot do.

Add a subcommand to the existing `Command` enum in `src/main.rs`:

```rust
/// Print the OpenAPI spec as JSON, for the docs build and for scripting.
Openapi,
```

It prints `ApiDoc::openapi()` as pretty JSON to stdout and exits 0. It touches no
database and reads no config — it must run on a machine with nothing provisioned,
because that is the machine CI runs on.

## 2. The committed spec, and the test that keeps it honest

The Pages workflow could run `cargo run -- openapi` on every docs change, but that
drags a full C++ FIX engine compile into what is otherwise a copy-and-render job.

Instead the generated spec is committed at `docs/openapi.json`, and a Rust test
asserts the committed file still matches what the code generates:

```rust
#[test]
fn the_committed_openapi_spec_is_current() {
    // The docs site renders docs/openapi.json rather than calling a running server,
    // so a stale file silently publishes a wrong API reference. Regenerate with
    // `cargo run -- openapi > docs/openapi.json`.
}
```

The failure message must name that exact regeneration command — a drift test that
does not tell you how to fix it wastes the reader's time.

This gives the checked-in-generated-file trade its usual shape: the file can go
stale, but only loudly. Change an endpoint without regenerating and `build.yml` goes
red.

## 3. The docs build

A single Node script, `site/build-docs.mjs`, run by the Pages workflow. No npm
dependencies of its own beyond mermaid-cli, which is invoked as a separate step.

### API reference → `api.html`

Reads `docs/openapi.json` and emits static HTML: operations grouped by tag, with
method, path, parameters, request body fields, response codes and a `curl` example
per operation.

The rendering logic ports directly from `ApiDocs.tsx`, which is already a set of
plain functions over the spec (`resolve`, `typeLabel`, `example`, `curlFor`,
`respColor`) plus small presentational components. The port replaces JSX with
template strings and Mantine components with the landing page's CSS vocabulary. It
is mechanical; the logic is not being redesigned.

Its visual language comes from `cockpit/src/components/apiTheme.tsx` — the terminal
palette the tokens page and the current API docs already share. Those values move
into `site/style.css` as the docs section of the existing stylesheet.

### Architecture → `architecture.html`

The four mermaid sources currently living as string constants in `Architecture.tsx`
(`OVERVIEW`, `ER`, `SEED`, `RUNTIME`) move to `site/diagrams/*.mmd` as ordinary
files, and become the single source of truth.

`site/architecture.html` is hand-written and carries the prose, the numbered `STEPS`
list and the `CARDS` content from the current page, with a placeholder element per
diagram:

```html
<figure class="diagram" data-diagram="overview"></figure>
```

The Pages workflow runs mermaid-cli over `site/diagrams/*.mmd` to produce SVG, and
`build-docs.mjs` inlines each SVG into its placeholder. The published page is pure
HTML with inline SVG — no mermaid at runtime.

The diagrams' current `classDef` colours are light-themed (`#e7edf3` on
`#3a4a5a`) because the cockpit renders them on a light card. On the dark site they
need the site's palette instead; the `.mmd` files carry the new `classDef` lines.

Note mermaid-cli pulls headless Chromium into the Pages job, which makes it
substantially slower than the current copy-only job. That is the accepted cost of
keeping the diagram source authoritative rather than committing generated SVG.

## 4. Removing it from the cockpit

- Delete `cockpit/src/pages/Architecture.tsx` and `cockpit/src/pages/ApiDocs.tsx`.
- In `cockpit/src/App.tsx`: drop the two `lazy(...)` imports (lines 19-22), the two
  `<Route>` blocks (`/docs/architecture` and `/api-docs`), and the `DOCS_NAV` array.
  Replace the navbar's Docs group with a single external link to
  `https://maxkuttner.github.io/openoms/architecture.html`, marked as leaving the app.
- Drop `mermaid` from `cockpit/package.json` and regenerate the lockfile. Verified:
  `Architecture.tsx` is its only importer in `cockpit/src/`.
- Keep `cockpit/src/components/apiTheme.tsx`: verified that `Api.tsx` (the
  trading-tokens page) imports `C`, `CmdLine` and `Eyebrow` from it, so it survives
  `ApiDocs.tsx` being deleted.
- `Suspense` and `Loader` may become unused in `App.tsx` — remove the imports if so.

The cockpit bundle should get materially smaller. Record the before/after size; if
dropping a megabyte of mermaid does not show up, something else is pulling it in.

## 5. Landing page

`site/index.html` gains links to both pages — in the existing footer, alongside
GitHub / Docs / Python client, and as a short "Docs" section in the body so someone
scanning the page finds them without reading the footer.

`pages.yml`'s `paths:` trigger grows to cover `docs/openapi.json` and
`site/diagrams/**`, so a spec or diagram change republishes the site.

## Consequences

- The API reference is only as fresh as `docs/openapi.json`. The drift test makes
  staleness loud in CI, but a contributor who never runs the test suite locally will
  see it only after pushing.
- The Pages job gets slower, from seconds to minutes, because of Chromium.
- Two rendering paths for the same OpenAPI spec now exist: this static one, and
  `/scalar` still served by the binary. They will drift in appearance. That is
  acceptable — `/scalar` is the interactive, try-it-against-my-server tool, and this
  is the read-it-before-you-install reference.
- The cockpit loses its only self-contained explanation of the instrument model.
  Operators with no internet access will not be able to reach the architecture page
  from the app; the external link will simply fail.
