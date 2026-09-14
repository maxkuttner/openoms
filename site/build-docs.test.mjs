import { test } from "node:test";
import assert from "node:assert/strict";
import { typeLabel, curlFor, escapeHtml, renderApi } from "./build-docs.mjs";

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

test("escapeHtml neutralises markup from the spec", () => {
  // Descriptions come from doc comments in Rust source; a stray angle bracket
  // must not become a tag in the generated page.
  assert.equal(escapeHtml(`<script>"x"&`), "&lt;script&gt;&quot;x&quot;&amp;");
});

test("renderApi emits one section per tag and documents each operation", () => {
  const html = renderApi(spec);
  assert.match(html, /orders/, "tag heading missing");
  assert.match(html, /\/orders\/submit/, "path missing");
  assert.match(html, /POST/, "method missing");
  assert.match(html, /422/, "response code missing");
  assert.doesNotMatch(html, /<script src|https?:\/\/(?!localhost)/, "no external references allowed");
});
