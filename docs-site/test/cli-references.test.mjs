// Every `oms …` command shown in the docs must be one the binary has: a
// reader copies these straight into a shell. Subcommands come from the clap
// enums in src/main.rs (variant names → kebab-case, as clap derives them).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const MAIN_RS = readFileSync(new URL("../../src/main.rs", import.meta.url), "utf8");
const DOCS = new URL("../src/content/docs/", import.meta.url).pathname;

const kebab = (s) => s.replace(/([a-z0-9])([A-Z])/g, "$1-$2").toLowerCase();

function variants(enumName) {
  const body = MAIN_RS.match(new RegExp(`enum ${enumName} \\{([\\s\\S]*?)\\n\\}`))?.[1];
  assert.ok(body, `enum ${enumName} not found in src/main.rs`);
  // A variant is an unindented-by-8 identifier at the enum's top level.
  return [...body.matchAll(/^ {4}([A-Z][A-Za-z]*)\b/gm)].map((m) => kebab(m[1]));
}

export function knownCommands() {
  const nested = { setup: "SetupCmd", database: "DatabaseCmd", config: "ConfigCmd" };
  const known = new Set();
  for (const top of variants("Command")) {
    if (nested[top]) for (const sub of variants(nested[top])) known.add(`${top} ${sub}`);
    else known.add(top);
  }
  return known;
}

// `oms`, `oms --flag`, `oms <cmd> [--flags]`, `oms <group> <sub> [--flags]`.
export function isKnown(line, known) {
  const words = line.trim().replace(/^\$\s+/, "").split(/\s+/);
  if (words[0] !== "oms") return true;
  const args = words.slice(1).filter((w) => !w.startsWith("#"));
  const end = args.findIndex((w) => w.startsWith("-") || w.startsWith("#"));
  const cmd = (end === -1 ? args : args.slice(0, end)).slice(0, 2);
  if (cmd.length === 0) return true;
  if (known.has(cmd[0])) return true;
  return known.has(cmd.join(" "));
}

function commandLines() {
  const lines = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (/\.mdx?$/.test(name)) {
        const text = readFileSync(p, "utf8");
        // Shell blocks only: in a ```python block `oms = OMS(...)` is a variable.
        for (const [, block] of text.matchAll(/```(?:sh|bash|shell|console)?\n([\s\S]*?)```/g)) {
          for (const l of block.split("\n")) {
            if (/^\s*(\$\s+)?oms(\s|$)/.test(l)) lines.push([p.slice(DOCS.length), l.trim()]);
          }
        }
      }
    }
  };
  walk(DOCS);
  return lines;
}

test("the subcommand list is read from src/main.rs", () => {
  const known = knownCommands();
  for (const c of ["init", "update", "openapi", "database migrate", "setup sync-broker", "config import-env"]) {
    assert.ok(known.has(c), `expected "${c}" among ${[...known].join(", ")}`);
  }
});

test("a made-up subcommand is rejected", () => {
  const known = knownCommands();
  assert.equal(isKnown("oms database frobnicate", known), false);
  assert.equal(isKnown("oms frobnicate", known), false);
  assert.equal(isKnown("oms database status", known), true);
  assert.equal(isKnown("oms --version", known), true);
  assert.equal(isKnown("oms update --check", known), true);
});

test("every oms command in the docs is a real subcommand", () => {
  const known = knownCommands();
  const lines = commandLines();
  assert.ok(lines.length > 0, "no oms commands found in the docs at all");
  const bad = lines.filter(([, l]) => !isKnown(l, known)).map(([f, l]) => `${f}: ${l}`);
  assert.deepEqual(bad, []);
});
