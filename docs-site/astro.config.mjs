// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";
import starlightOpenAPI, { openAPISidebarGroups } from "starlight-openapi";

// Served by GitHub Pages at the custom domain, from the root: https://openoms.dev/
export default defineConfig({
  site: "https://openoms.dev",
  integrations: [
    starlight({
      title: "openoms",
      description: "An open source multi-client order management system.",
      logo: { src: "./src/assets/logo.svg" },
      favicon: "/favicon.svg",
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/maxkuttner/openoms" }],
      editLink: { baseUrl: "https://github.com/maxkuttner/openoms/edit/main/docs-site/" },
      customCss: ["@fontsource-variable/geist", "@fontsource-variable/geist-mono", "./src/styles/theme.css"],
      components: { Header: "./src/components/Header.astro" },
      plugins: [
        // Generated from the committed spec — never hand-written, so it cannot
        // drift from the server (the_committed_openapi_spec_is_current keeps
        // docs/openapi.json itself in step with the code).
        // .generated/openapi.json is docs/openapi.json plus cURL, Python and Rust
        // samples per operation (scripts/openapi-samples.mjs, run by prebuild).
        starlightOpenAPI([{ base: "api", schema: "./.generated/openapi.json", sidebar: { label: "API Reference" } }]),
        // Generated API pages are invisible to the validator; test/dist.test.mjs
        // checks links into them against the built files instead.
        starlightLinksValidator({ exclude: ["/api", "/api/**"] }),
      ],
      sidebar: [
        {
          label: "Getting Started",
          items: [
            "getting-started/installation",
            "getting-started",
          ],
        },
        {
          label: "Configuration",
          items: ["configuration/reference", "configuration/file-locations", "configuration/login"],
        },
        { label: "Concepts", items: ["concepts/architecture"] },
        { label: "Strategies", items: ["strategies/fix", "strategies/rest", "strategies/colocated"] },
        {
          label: "Integrations",
          items: [
            "integrations/alpaca",
            "integrations/binance",
            "integrations/interactive-brokers",
            "integrations/bybit",
            "integrations/coinbase",
            "integrations/okx",
            "integrations/databento",
          ],
        },
        {
          label: "Operations",
          items: [
            "operations/updating",
            "operations/service",
            "operations/database",
            "operations/master-key-and-backups",
            "operations/troubleshooting",
          ],
        },
        ...openAPISidebarGroups,
      ],
    }),
  ],
});
