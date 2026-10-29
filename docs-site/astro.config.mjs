// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLinksValidator from "starlight-links-validator";

// Served as a GitHub Pages project site: https://maxkuttner.github.io/openoms/
export default defineConfig({
  site: "https://maxkuttner.github.io",
  base: "/openoms",
  integrations: [
    starlight({
      title: "openoms",
      description: "An open source multi-client order management system.",
      logo: { src: "./src/assets/logo.svg" },
      favicon: "/favicon.svg",
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/maxkuttner/openoms" }],
      editLink: { baseUrl: "https://github.com/maxkuttner/openoms/edit/main/docs-site/" },
      customCss: ["./src/styles/theme.css"],
      plugins: [starlightLinksValidator()],
      sidebar: [
        {
          label: "Getting Started",
          items: [
            "getting-started/installation",
            "getting-started/first-run",
            "getting-started/first-paper-trade",
            "getting-started/first-api-call",
          ],
        },
        {
          label: "Configuration",
          items: ["configuration/reference", "configuration/file-locations", "configuration/login"],
        },
        { label: "Concepts", items: ["concepts/architecture"] },
        {
          label: "Integrations",
          items: [
            "integrations/alpaca",
            "integrations/binance",
            "integrations/interactive-brokers",
            "integrations/bybit",
            "integrations/databento",
          ],
        },
        { label: "Tutorials", items: ["tutorials/opra-marks-pnl"] },
        {
          label: "Operations",
          items: ["operations/updating", "operations/service", "operations/master-key-and-backups"],
        },
      ],
    }),
  ],
});
