#!/usr/bin/env node
//
// Renders the committed OpenAPI spec (docs/openapi.json) into a static
// api.html. The published site has no server to ask for the spec at
// request time — unlike the cockpit's ApiDocs page, which fetches
// /api-docs/openapi.json from a running oms — so this bakes the same
// reference into a plain HTML page at build time.
//
// Ported from cockpit/src/pages/ApiDocs.tsx, which was already plain
// functions over the spec: resolve/typeLabel/example/curlFor/respColor
// carry over near verbatim, JSX becomes template strings, and Mantine's
// styling becomes the site's own CSS classes (see the "docs" section
// appended to site/style.css).
//
// No npm dependencies: node:fs, node:path and node:url only.

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

const METHODS = ["get", "post", "put", "patch", "delete"];

// -- spec helpers (ported from ApiDocs.tsx) ---------------------------------

const refName = (ref) => ref?.split("/").pop();

export function resolve(spec, s) {
  if (!s) return s;
  if (s.$ref) return spec.components?.schemas?.[refName(s.$ref) ?? ""] ?? {};
  return s;
}

export function typeLabel(spec, s) {
  const r = resolve(spec, s);
  if (!r) return "—";
  if (r.enum) return r.enum.join(" | ");
  if (r.type === "array") return `${typeLabel(spec, r.items)}[]`;
  const base = r.type ?? (r.properties ? "object" : "—");
  // A format, when the spec gives one, is more specific than the bare type
  // (e.g. "int64" says more than "integer"), so prefer it over the base.
  return r.format ?? base;
}

// A concrete example value for a schema, used to render a runnable request body.
export function example(spec, s) {
  const r = resolve(spec, s);
  if (!r) return null;
  if (r.example !== undefined) return r.example;
  if (r.enum) return r.enum[0];
  switch (r.type) {
    case "number":
    case "integer":
      return 0;
    case "boolean":
      return false;
    case "array":
      return [example(spec, r.items)];
    case "object": {
      const o = {};
      for (const [k, v] of Object.entries(r.properties ?? {})) o[k] = example(spec, v);
      return o;
    }
    default:
      return r.nullable ? null : "";
  }
}

const bodyOf = (spec, op) => resolve(spec, op.requestBody?.content?.["application/json"]?.schema);

export function curlFor(spec, method, path_, op) {
  const lines = [`curl "$OMS_URL${path_}"`];
  if (method !== "get") lines.push(`  -X ${method.toUpperCase()}`);
  if (op.security?.length) lines.push(`  --header "Authorization: Bearer $OMS_TOKEN"`);
  const body = bodyOf(spec, op);
  if (body) {
    lines.push(`  --header "Content-Type: application/json"`);
    lines.push(`  --data '${JSON.stringify(example(spec, body), null, 2)}'`);
  }
  return lines.join(" \\\n");
}

// Status-code colour category. Green for 2xx, red for 5xx, amber otherwise
// (matches cockpit/src/components/apiTheme.tsx's C.green/C.amber/C.red).
export function respColor(code) {
  const c = String(code);
  if (c.startsWith("2")) return "green";
  if (c.startsWith("5")) return "red";
  return "amber";
}

// -- HTML rendering -----------------------------------------------------

// Every string interpolated from the spec goes through this. Descriptions
// and summaries originate in Rust doc comments and must never become markup.
export function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

const slug = (m, p) => `${m}-${p}`.replace(/[^a-z0-9]+/gi, "-").toLowerCase();

// Buckets operations by their first tag, matching ApiDocsPage's grouping.
// Operations with no tag land in a final "other" group.
function groupOperations(spec) {
  const groups = [];
  const byTag = new Map();
  for (const [p, ops] of Object.entries(spec.paths ?? {})) {
    for (const m of METHODS) {
      const op = ops[m];
      if (!op) continue;
      const tag = op.tags?.[0] ?? "other";
      if (!byTag.has(tag)) {
        byTag.set(tag, []);
        groups.push([tag, byTag.get(tag)]);
      }
      byTag.get(tag).push({ method: m, path: p, op });
    }
  }
  return groups;
}

function renderRow({ name, type, required, example: ex, desc }) {
  const hasExample = ex !== undefined && ex !== null && ex !== "";
  const exampleHtml = hasExample
    ? `<div class="docs-row-example">e.g. ${escapeHtml(typeof ex === "string" ? ex : JSON.stringify(ex))}</div>`
    : "";
  return `<div class="docs-row">
    <div class="docs-row-name">
      <div class="docs-row-name-text">${escapeHtml(name)}${required ? '<span class="docs-required"> *</span>' : ""}</div>
      <div class="docs-row-type">${escapeHtml(type)}</div>
    </div>
    <div class="docs-row-body">
      ${desc ? `<div class="docs-row-desc">${escapeHtml(desc)}</div>` : ""}
      ${exampleHtml}
    </div>
  </div>`;
}

function renderPanel(title, innerHtml) {
  return `<div class="docs-panel">
    <div class="docs-panel-title">${escapeHtml(title)}</div>
    ${innerHtml}
  </div>`;
}

function renderMethodBadge(method) {
  return `<span class="method method-${escapeHtml(method)}">${escapeHtml(method.toUpperCase())}</span>`;
}

function renderOperation(spec, entry) {
  const { method, path: p, op } = entry;
  const id = slug(method, p);
  const params = op.parameters ?? [];
  const body = bodyOf(spec, op);
  const bodyFields = Object.entries(body?.properties ?? {});
  const schemes = Array.from(new Set((op.security ?? []).flatMap((m) => Object.keys(m))));

  const summaryHtml =
    op.summary || op.description
      ? `<p class="docs-op-summary">${escapeHtml(op.summary || op.description)}</p>`
      : "";

  const authHtml = schemes.length
    ? `<div class="docs-op-auth"><span class="docs-eyebrow">Auth</span>${schemes
        .map((s) => `<span class="docs-auth-scheme">${escapeHtml(s)}</span>`)
        .join("")}</div>`
    : "";

  const paramsHtml = params.length
    ? renderPanel(
        "Parameters",
        params
          .map((prm) =>
            renderRow({
              name: prm.name,
              type: `${prm.in} · ${typeLabel(spec, prm.schema)}`,
              required: prm.required,
              desc: prm.description,
            }),
          )
          .join(""),
      )
    : "";

  const bodyHtml = bodyFields.length
    ? renderPanel(
        "Request body",
        (body?.description
          ? `<div class="docs-row-desc docs-body-desc">${escapeHtml(body.description)}</div>`
          : "") +
          bodyFields
            .map(([name, s]) =>
              renderRow({
                name,
                type: typeLabel(spec, s),
                required: body?.required?.includes(name),
                example: resolve(spec, s)?.example,
                desc: resolve(spec, s)?.description,
              }),
            )
            .join(""),
      )
    : "";

  const responsesHtml = op.responses
    ? renderPanel(
        "Responses",
        Object.entries(op.responses)
          .map(
            ([code, r]) =>
              `<div class="docs-resp-row"><span class="docs-resp-code resp-${respColor(code)}">${escapeHtml(
                code,
              )}</span><span class="docs-resp-desc">${escapeHtml(r?.description ?? "")}</span></div>`,
          )
          .join(""),
      )
    : "";

  const curl = curlFor(spec, method, p, op);

  return `<div class="docs-op" id="${id}">
    <div class="docs-op-head">
      ${renderMethodBadge(method)}
      <span class="docs-op-path">${escapeHtml(p)}</span>
    </div>
    ${summaryHtml}
    ${authHtml}
    ${paramsHtml}
    ${bodyHtml}
    ${responsesHtml}
    <div class="docs-panel docs-example">
      <div class="docs-panel-title">Example</div>
      <pre class="docs-curl"><code>${escapeHtml(curl)}</code></pre>
    </div>
  </div>`;
}

// Full HTML document: same <head> shape as site/index.html, a header
// linking back to it, then one <section> per tag with one block per
// operation (method badge, path, summary, parameters, request-body
// fields, response codes, curl example).
export function renderApi(spec) {
  const groups = groupOperations(spec);
  const schemes = spec.components?.securitySchemes ?? {};

  const navHtml = groups
    .map(
      ([tag, entries]) => `<div class="docs-nav-group">
        <div class="docs-eyebrow">${escapeHtml(tag)}</div>
        ${entries
          .map(
            (e) =>
              `<a class="docs-nav-link" href="#${slug(e.method, e.path)}">${renderMethodBadge(
                e.method,
              )}<span class="docs-nav-path">${escapeHtml(e.path)}</span></a>`,
          )
          .join("")}
      </div>`,
    )
    .join("");

  const authSection = Object.keys(schemes).length
    ? renderPanel(
        "Authentication",
        Object.entries(schemes)
          .map(([name, s]) => renderRow({ name, type: `http · ${s?.scheme ?? ""}`, desc: s?.description }))
          .join(""),
      )
    : "";

  const groupsHtml = groups
    .map(
      ([tag, entries]) => `<section class="docs-section">
        <h2 class="docs-tag">${escapeHtml(tag)}</h2>
        ${entries.map((e) => renderOperation(spec, e)).join("")}
      </section>`,
    )
    .join("");

  const title = spec.info?.title ?? "OMS API";
  const version = spec.info?.version ?? "";

  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>openoms — API reference</title>
    <link rel="icon" href="favicon.svg" />
    <link rel="stylesheet" href="style.css" />
  </head>
  <body>
    <header class="docs-header">
      <p class="docs-back"><a href="index.html">&larr; openoms</a></p>
      <h1>${escapeHtml(title)} <span class="docs-version">v${escapeHtml(version)}</span></h1>
      <p class="tagline">
        Point clients at your OMS host (<code>$OMS_URL</code>). Mint a bearer token on the
        Tokens tab and send it as <code>Authorization: Bearer $OMS_TOKEN</code>.
      </p>
    </header>
    <main class="docs-main">
      <nav class="docs-nav">${navHtml}</nav>
      <div class="docs-content">
        ${authSection}
        ${groupsHtml}
      </div>
    </main>
  </body>
</html>
`;
}

// -- entry point ----------------------------------------------------------

function main() {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const repoRoot = path.resolve(here, "..");
  const specPath = path.join(repoRoot, "docs", "openapi.json");
  const spec = JSON.parse(fs.readFileSync(specPath, "utf8"));

  const outDir = path.resolve(process.argv[2] ?? "_site");
  fs.mkdirSync(outDir, { recursive: true });
  fs.writeFileSync(path.join(outDir, "api.html"), renderApi(spec));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
