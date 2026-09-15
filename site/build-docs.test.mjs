import { test } from "node:test";
import assert from "node:assert/strict";
import { typeLabel, curlFor, escapeHtml, renderApi, inlineDiagrams } from "./build-docs.mjs";

const spec = {
  openapi: "3.0.3",
  paths: {
    "/orders/submit": {
      post: {
        tags: ["orders"],
        summary: "Submit an order",
        requestBody: {
          content: {
            "application/json": {
              schema: { $ref: "#/components/schemas/SubmitOrderRequest" },
            },
          },
        },
        responses: { 200: { description: "accepted" }, 422: { description: "rejected" } },
      },
    },
  },
  components: {
    schemas: {
      SubmitOrderRequest: {
        type: "object",
        required: ["symbol"],
        properties: {
          symbol: { type: "string", example: "SPY@ARCX" },
          quantity: { type: "integer", format: "int64" },
        },
      },
    },
  },
};

test("typeLabel follows a $ref and names the type", () => {
  assert.equal(typeLabel(spec, { $ref: "#/components/schemas/SubmitOrderRequest" }), "object");
  assert.equal(typeLabel(spec, { type: "integer", format: "int64" }), "int64");
  assert.equal(typeLabel(spec, { type: "array", items: { type: "string" } }), "string[]");
});

test("curlFor builds a request against the documented path", () => {
  const curl = curlFor(spec, "post", "/orders/submit", spec.paths["/orders/submit"].post);
  assert.match(curl, /curl/);
  assert.match(curl, /-X POST/);
  assert.match(curl, /\/orders\/submit/);
  assert.match(curl, /SPY@ARCX/, "the body should use the schema's example");
});

test("renderApi shows a body panel for a map-shaped (additionalProperties) request body", () => {
  // Mirrors CredentialSubmission: no named `properties`, so bodyFields is
  // empty, but the body itself is real and its description must not be
  // dropped — and the curl example must not read as an empty, safe-to-run '{}'.
  const credSpec = {
    ...spec,
    paths: {
      "/admin/broker-connections/{code}/credentials": {
        put: {
          tags: ["admin"],
          summary: "Set broker credentials",
          requestBody: {
            required: true,
            content: {
              "application/json": {
                schema: { $ref: "#/components/schemas/CredentialSubmission" },
              },
            },
          },
          responses: { 200: { description: "ok" } },
        },
      },
    },
    components: {
      schemas: {
        CredentialSubmission: {
          type: "object",
          description: "A submitted credential form: field name to raw string value.",
          additionalProperties: { type: "string" },
        },
      },
    },
  };

  const html = renderApi(credSpec);
  assert.match(html, /Request body/, "no body panel rendered for a map-shaped body");
  assert.match(
    html,
    /A submitted credential form: field name to raw string value\./,
    "the body's description was dropped",
  );
  assert.match(html, /any field name/i, "no indication the body accepts arbitrary field names");
});

test("escapeHtml neutralises markup from the spec", () => {
  // Descriptions come from doc comments in Rust source; a stray angle bracket
  // must not become a tag in the generated page.
  assert.equal(escapeHtml(`<script>"x"&`), "&lt;script&gt;&quot;x&quot;&amp;");
  // Not just double quotes: every interpolation site in this file happens to
  // use double-quoted attributes today, but that's an invariant this
  // function shouldn't rely on.
  assert.equal(escapeHtml(`it's <b>"bold"</b>`), "it&#39;s &lt;b&gt;&quot;bold&quot;&lt;/b&gt;");
});

test("renderApi emits one section per tag and documents each operation", () => {
  const html = renderApi(spec);
  assert.match(html, /orders/, "tag heading missing");
  assert.match(html, /\/orders\/submit/, "path missing");
  assert.match(html, /POST/, "method missing");
  assert.match(html, /422/, "response code missing");
  assert.doesNotMatch(html, /<script src|https?:\/\/(?!localhost)/, "no external references allowed");
});

test("inlineDiagrams puts each SVG inside its own placeholder", () => {
  const html = `<figure class="diagram" data-diagram="overview"></figure>
                <figure class="diagram" data-diagram="er"></figure>`;
  const svgs = { overview: "<svg id='a'></svg>", er: "<svg id='b'></svg>" };
  const out = inlineDiagrams(html, svgs);

  assert.match(out, /data-diagram="overview"[^]*?<svg id='a'>/);
  assert.match(out, /data-diagram="er"[^]*?<svg id='b'>/);
  assert.doesNotMatch(out, /data-diagram="overview"[^]*?<svg id='b'>[^]*?<\/figure>\s*<figure/);
});

test("inlineDiagrams fails loudly on a placeholder with no SVG", () => {
  // A silently empty figure would publish an architecture page with a hole in it.
  assert.throws(
    () => inlineDiagrams(`<figure class="diagram" data-diagram="runtime"></figure>`, {}),
    /runtime/,
  );
});
