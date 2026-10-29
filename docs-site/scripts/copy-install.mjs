// The site serves the repo's install.sh at /openoms/install.sh — the URL the
// README, install-smoke.yml and every saved copy-paste use. The root copy is
// canonical; this puts it into public/ so Astro ships it byte-for-byte.
import { copyFileSync } from "node:fs";

copyFileSync(new URL("../../install.sh", import.meta.url), new URL("../public/install.sh", import.meta.url));
