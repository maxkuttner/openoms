import { useMemo } from "react";
import { Box, Group, Loader, Stack, Text } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { C, CmdLine, Eyebrow } from "../components/apiTheme";
import { API_BASE } from "../api/client";

// A small, self-documenting API reference rendered directly from the OMS
// OpenAPI spec (/api-docs/openapi.json). Databento-flavoured: no framework,
// no 2 MB bundle — just the spec, grouped by tag, in the terminal style.

type Schema = {
  type?: string;
  format?: string;
  description?: string;
  example?: unknown;
  nullable?: boolean;
  enum?: string[];
  $ref?: string;
  properties?: Record<string, Schema>;
  required?: string[];
  items?: Schema;
};
type Param = { name: string; in: string; description?: string; required?: boolean; schema?: Schema };
type Op = {
  tags?: string[];
  summary?: string | null;
  description?: string;
  parameters?: Param[];
  requestBody?: { content?: Record<string, { schema?: Schema }> };
  responses?: Record<string, { description?: string }>;
  security?: Array<Record<string, string[]>>;
};
type Spec = {
  info?: { title?: string; version?: string; description?: string };
  paths?: Record<string, Record<string, Op>>;
  components?: {
    schemas?: Record<string, Schema>;
    securitySchemes?: Record<string, { scheme?: string; description?: string }>;
  };
};

const METHODS = ["get", "post", "put", "patch", "delete"] as const;
const METHOD_COLOR: Record<string, string> = {
  get: C.green, post: C.amber, put: C.amber, patch: C.amber, delete: C.red,
};

type Entry = { method: string; path: string; op: Op; id: string };

const slug = (m: string, p: string) => `${m}-${p}`.replace(/[^a-z0-9]+/gi, "-").toLowerCase();
const refName = (ref?: string) => ref?.split("/").pop();

function resolve(spec: Spec, s?: Schema): Schema | undefined {
  if (!s) return s;
  if (s.$ref) return spec.components?.schemas?.[refName(s.$ref) ?? ""] ?? {};
  return s;
}

function typeLabel(spec: Spec, s?: Schema): string {
  const r = resolve(spec, s);
  if (!r) return "—";
  if (r.enum) return r.enum.join(" | ");
  if (r.type === "array") return `${typeLabel(spec, r.items)}[]`;
  const base = r.type ?? (r.properties ? "object" : "—");
  return r.format ? `${base} · ${r.format}` : base;
}

// A concrete example value for a schema, used to render a runnable request body.
function example(spec: Spec, s?: Schema): unknown {
  const r = resolve(spec, s);
  if (!r) return null;
  if (r.example !== undefined) return r.example;
  if (r.enum) return r.enum[0];
  switch (r.type) {
    case "number":
    case "integer": return 0;
    case "boolean": return false;
    case "array": return [example(spec, r.items)];
    case "object": {
      const o: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(r.properties ?? {})) o[k] = example(spec, v);
      return o;
    }
    default: return r.nullable ? null : "";
  }
}

const bodyOf = (spec: Spec, op: Op) =>
  resolve(spec, op.requestBody?.content?.["application/json"]?.schema);

function curlFor(spec: Spec, method: string, path: string, op: Op): string {
  const lines = [`curl "$OMS_URL${path}"`];
  if (method !== "get") lines.push(`  --request ${method.toUpperCase()}`);
  if (op.security?.length) lines.push(`  --header "Authorization: Bearer $OMS_TOKEN"`);
  const body = bodyOf(spec, op);
  if (body) {
    lines.push(`  --header "Content-Type: application/json"`);
    lines.push(`  --data '${JSON.stringify(example(spec, body), null, 2)}'`);
  }
  return lines.join(" \\\n");
}

function respColor(code: string): string {
  if (code.startsWith("2")) return C.green;
  if (code.startsWith("5")) return C.red;
  return C.amber;
}

function MethodBadge({ method }: { method: string }) {
  return (
    <Text
      component="span"
      fz={10}
      fw={800}
      style={{
        color: METHOD_COLOR[method] ?? C.muted,
        border: `1px solid ${METHOD_COLOR[method] ?? C.border}`,
        borderRadius: 4,
        padding: "2px 7px",
        letterSpacing: 1,
        textTransform: "uppercase",
        flex: "none",
      }}
    >
      {method}
    </Text>
  );
}

// Field / parameter table row.
function Row({ name, type, required, example: ex, desc }: {
  name: string; type: string; required?: boolean; example?: unknown; desc?: string;
}) {
  return (
    <Group px={14} py={9} wrap="nowrap" align="flex-start" style={{ borderTop: `1px solid ${C.border}` }}>
      <Box w={150} style={{ flex: "none" }}>
        <Text fz={12.5} c={C.ink} style={{ fontFamily: "monospace" }}>
          {name}
          {required && <Text component="span" c={C.amber} inherit> *</Text>}
        </Text>
        <Text fz={11} c={C.faint} style={{ fontFamily: "monospace" }}>{type}</Text>
      </Box>
      <Box style={{ flex: 1, minWidth: 0 }}>
        {desc && <Text fz={12} c={C.muted}>{desc}</Text>}
        {ex !== undefined && ex !== null && ex !== "" && (
          <Text fz={11} c={C.faint} mt={desc ? 3 : 0} style={{ fontFamily: "monospace" }}>
            e.g. {typeof ex === "string" ? ex : JSON.stringify(ex)}
          </Text>
        )}
      </Box>
    </Group>
  );
}

function Panel({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <Box style={{ border: `1px solid ${C.border}`, background: C.panel, borderRadius: 6, overflow: "hidden" }}>
      <Box px={14} py={7} style={{ borderBottom: `1px solid ${C.border}` }}>
        <Eyebrow>{title}</Eyebrow>
      </Box>
      {children}
    </Box>
  );
}

function Operation({ spec, entry }: { spec: Spec; entry: Entry }) {
  const { method, path, op, id } = entry;
  const params = op.parameters ?? [];
  const body = bodyOf(spec, op);
  const bodyFields = Object.entries(body?.properties ?? {});
  const schemes = Array.from(new Set((op.security ?? []).flatMap((m) => Object.keys(m))));

  return (
    <Stack gap={12} id={id} style={{ scrollMarginTop: 16 }}>
      <Group gap={10} wrap="nowrap" align="center">
        <MethodBadge method={method} />
        <Text fz={15} c={C.ink} style={{ fontFamily: "monospace", wordBreak: "break-all" }}>{path}</Text>
      </Group>
      {(op.summary || op.description) && (
        <Text fz={13} c={C.muted} style={{ whiteSpace: "pre-wrap" }}>{op.summary || op.description}</Text>
      )}
      {schemes.length > 0 && (
        <Group gap={6}>
          <Eyebrow>Auth</Eyebrow>
          {schemes.map((s) => (
            <Text key={s} fz={11} c={C.green} style={{ fontFamily: "monospace" }}>{s}</Text>
          ))}
        </Group>
      )}

      {params.length > 0 && (
        <Panel title="Parameters">
          {params.map((p) => (
            <Row key={p.name} name={p.name} type={`${p.in} · ${typeLabel(spec, p.schema)}`}
              required={p.required} desc={p.description} />
          ))}
        </Panel>
      )}

      {bodyFields.length > 0 && (
        <Panel title="Request body">
          {body?.description && (
            <Text fz={12} c={C.muted} px={14} py={9} style={{ whiteSpace: "pre-wrap", borderTop: `1px solid ${C.border}` }}>
              {body.description}
            </Text>
          )}
          {bodyFields.map(([name, s]) => (
            <Row key={name} name={name} type={typeLabel(spec, s)}
              required={body?.required?.includes(name)}
              example={resolve(spec, s)?.example}
              desc={resolve(spec, s)?.description} />
          ))}
        </Panel>
      )}

      {op.responses && (
        <Panel title="Responses">
          {Object.entries(op.responses).map(([code, r]) => (
            <Group key={code} px={14} py={7} wrap="nowrap" style={{ borderTop: `1px solid ${C.border}` }}>
              <Text w={44} fz={12.5} fw={700} c={respColor(code)} style={{ fontFamily: "monospace", flex: "none" }}>{code}</Text>
              <Text fz={12.5} c={C.muted}>{r.description}</Text>
            </Group>
          ))}
        </Panel>
      )}

      <Box style={{ border: `1px solid ${C.border}`, background: C.inset, borderRadius: 6, overflow: "hidden" }}>
        <Box px={14} py={7} style={{ borderBottom: `1px solid ${C.border}` }}>
          <Eyebrow>Example</Eyebrow>
        </Box>
        <Box py={4}>
          <CmdLine value={curlFor(spec, method, path, op)} display={curlFor(spec, method, path, op)} />
        </Box>
      </Box>
    </Stack>
  );
}

export function ApiDocsPage() {
  const { data: spec, isLoading, error } = useQuery<Spec>({
    queryKey: ["openapi"],
    queryFn: async () => {
      const r = await fetch(`${API_BASE}/api-docs/openapi.json`);
      if (!r.ok) throw new Error(`spec ${r.status}`);
      return r.json();
    },
    staleTime: 5 * 60_000,
  });

  const groups = useMemo(() => {
    const g: Array<[string, Entry[]]> = [];
    const byTag = new Map<string, Entry[]>();
    for (const [path, ops] of Object.entries(spec?.paths ?? {})) {
      for (const m of METHODS) {
        const op = (ops as Record<string, Op>)[m];
        if (!op) continue;
        const tag = op.tags?.[0] ?? "other";
        if (!byTag.has(tag)) { byTag.set(tag, []); g.push([tag, byTag.get(tag)!]); }
        byTag.get(tag)!.push({ method: m, path, op, id: slug(m, path) });
      }
    }
    return g;
  }, [spec]);

  if (isLoading) return <Group p={24}><Loader size="sm" color="depth" /></Group>;
  if (error || !spec) return <Text c={C.red} p={24}>Failed to load API spec.</Text>;

  const schemes = spec.components?.securitySchemes ?? {};

  return (
    <Group align="flex-start" gap={28} wrap="nowrap">
      {/* nav */}
      <Box style={{ position: "sticky", top: 16, width: 210, flex: "none", maxHeight: "calc(100vh - 90px)", overflowY: "auto" }}>
        <Stack gap={16}>
          {groups.map(([tag, entries]) => (
            <div key={tag}>
              <Eyebrow>{tag}</Eyebrow>
              <Stack gap={2} mt={6}>
                {entries.map((e) => (
                  <a key={e.id} href={`#${e.id}`} style={{ textDecoration: "none" }}>
                    <Group gap={7} wrap="nowrap" style={{ padding: "2px 0" }}>
                      <Text fz={9} fw={800} c={METHOD_COLOR[e.method] ?? C.muted} w={30} style={{ flex: "none", textTransform: "uppercase" }}>
                        {e.method}
                      </Text>
                      <Text fz={11.5} c={C.muted} truncate style={{ fontFamily: "monospace" }}>{e.path}</Text>
                    </Group>
                  </a>
                ))}
              </Stack>
            </div>
          ))}
        </Stack>
      </Box>

      {/* content */}
      <Stack gap={34} style={{ flex: 1, minWidth: 0 }} maw={820}>
        <div>
          <Eyebrow>API reference</Eyebrow>
          <Text fz={26} fw={800} c={C.ink} mt={2} style={{ letterSpacing: -0.5 }}>
            {spec.info?.title ?? "OMS API"}{" "}
            <Text component="span" fz={13} c={C.faint} fw={500}>v{spec.info?.version}</Text>
          </Text>
          <Text fz={13} c={C.muted} mt={6} maw={640}>
            Point clients at your OMS host (<Text component="span" c={C.green} inherit style={{ fontFamily: "monospace" }}>$OMS_URL</Text>).
            Mint a bearer token on the <Text component="span" c={C.green} inherit>Tokens</Text> tab and send it as
            <Text component="span" c={C.green} inherit style={{ fontFamily: "monospace" }}> Authorization: Bearer $OMS_TOKEN</Text>.
          </Text>
        </div>

        {Object.keys(schemes).length > 0 && (
          <Panel title="Authentication">
            {Object.entries(schemes).map(([name, s]) => (
              <Row key={name} name={name} type={`http · ${s.scheme}`} desc={s.description} />
            ))}
          </Panel>
        )}

        {groups.map(([tag, entries]) => (
          <Stack key={tag} gap={22}>
            <Text fz={13} fw={700} c={C.ink} style={{ textTransform: "uppercase", letterSpacing: 1 }}>{tag}</Text>
            {entries.map((e) => <Operation key={e.id} spec={spec} entry={e} />)}
          </Stack>
        ))}
      </Stack>
    </Group>
  );
}
