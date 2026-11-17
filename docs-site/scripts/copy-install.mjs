// The site serves the repo's install.sh at /install.sh and compose.yaml at
// /compose.yaml — the URLs the README, the install guide, install-smoke.yml and
// every saved copy-paste use. The root copies are canonical; this puts them into
// public/ so Astro ships them byte-for-byte.
import { copyFileSync } from "node:fs";

for (const f of ["install.sh", "compose.yaml"]) {
  copyFileSync(new URL(`../../${f}`, import.meta.url), new URL(`../public/${f}`, import.meta.url));
}
