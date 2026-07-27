import { useState } from "react";
import {
  Box,
  Button,
  CopyButton,
  Group,
  Loader,
  Select,
  Stack,
  Text,
  TextInput,
  Tooltip,
} from "@mantine/core";
import { Link } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { useList, notifyError, notifyOk } from "../api/hooks";
import type { Principal, Portfolio, TradingTokenCreated, TradingTokenRow } from "../api/types";

const PATH = "/admin/trading-tokens";

// Brand tokens (theme.ts): green "bids", amber "asks", near-black terminal ink.
const C = {
  panel: "#1a1e26",
  inset: "#0b0d10",
  border: "#2a2f38",
  ink: "#eff1f4",
  muted: "#9aa3af",
  faint: "#6b7280",
  green: "#22ae6c",
  amber: "#ce9a3b",
};

function ago(iso: string): string {
  const s = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86400)}d`;
}

function Eyebrow({ children }: { children: React.ReactNode }) {
  return (
    <Text fz={10} c={C.faint} style={{ letterSpacing: 2, textTransform: "uppercase" }}>
      {children}
    </Text>
  );
}

/** A single `$`-prompted terminal line with an inline copy affordance. */
function CmdLine({ value, display }: { value: string; display: React.ReactNode }) {
  return (
    <Group gap={10} wrap="nowrap" align="flex-start" style={{ padding: "6px 12px" }}>
      <Text fz={13} c={C.green} style={{ userSelect: "none", lineHeight: 1.6 }}>$</Text>
      <Box style={{ flex: 1, minWidth: 0, whiteSpace: "pre-wrap", wordBreak: "break-all", fontSize: 12.5, lineHeight: 1.6, color: C.ink }}>
        {display}
      </Box>
      <CopyButton value={value}>
        {({ copied, copy }) => (
          <Tooltip label={copied ? "Copied" : "Copy"} withArrow>
            <Text
              onClick={copy}
              fz={11}
              c={copied ? C.green : C.faint}
              style={{ cursor: "pointer", userSelect: "none", lineHeight: 1.6 }}
            >
              {copied ? "copied" : "copy"}
            </Text>
          </Tooltip>
        )}
      </CopyButton>
    </Group>
  );
}

export function ApiPage() {
  const qc = useQueryClient();
  const { data: tokens, isLoading } = useList<TradingTokenRow>(PATH);
  const { data: principals } = useList<Principal>("/admin/principals");
  const { data: portfolios } = useList<Portfolio>("/admin/portfolios");

  const [principalId, setPrincipalId] = useState<string | null>(null);
  const [portfolioId, setPortfolioId] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<TradingTokenCreated | null>(null);

  const principalOpts = (principals ?? []).map((p) => ({
    value: p.id,
    label: p.display_name ? `${p.display_name} · ${p.code}` : p.code,
  }));
  const portfolioOpts = (portfolios ?? []).map((p) => ({
    value: p.id,
    label: p.name ? `${p.name} · ${p.code}` : p.code,
  }));

  const canGenerate = !!principalId;

  async function generate() {
    setCreating(true);
    try {
      const res = await api.post<TradingTokenCreated>(PATH, {
        principal_id: principalId,
        portfolio_id: portfolioId,
        label: label.trim() || null,
      });
      setCreated(res);
      setLabel("");
      qc.invalidateQueries({ queryKey: [PATH] });
      notifyOk("Token created");
    } catch (e) {
      notifyError(e);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(keyId: string) {
    try {
      await api.del(`${PATH}/${keyId}`);
      qc.invalidateQueries({ queryKey: [PATH] });
      if (created?.key_id === keyId) setCreated(null);
      notifyOk("Token revoked");
    } catch (e) {
      notifyError(e);
    }
  }

  const curl = created
    ? `curl "$OMS_URL/orders/submit" -H "Authorization: Bearer ${created.token}" -H "Content-Type: application/json" -d @order.json`
    : "";

  const inputStyles = {
    input: { background: C.inset, borderColor: C.border, color: C.ink, fontSize: 13 },
    label: { color: C.muted, fontSize: 11, marginBottom: 4 },
  };

  return (
    <Stack gap={28} maw={880}>
      <div>
        <Eyebrow>API access</Eyebrow>
        <Text fz={26} fw={800} c={C.ink} mt={2} style={{ letterSpacing: -0.5 }}>
          API tokens
        </Text>
        <Text fz={13} c={C.muted} mt={6} maw={620}>
          A token is a single bearer credential for a bot or strategy. It belongs to a
          principal; what it can trade follows that principal's portfolio grants. Point
          clients at your OMS host and send{" "}
          <Text component="span" c={C.green} inherit>Authorization: Bearer &lt;token&gt;</Text>.
        </Text>
      </div>

      {/* ── Create ─────────────────────────────────────────── */}
      <Box style={{ border: `1px solid ${C.border}`, background: C.panel, borderRadius: 6 }}>
        <Box style={{ padding: "10px 16px", borderBottom: `1px solid ${C.border}` }}>
          <Eyebrow>Create token</Eyebrow>
        </Box>
        <Stack gap={14} p={16}>
          {principalOpts.length === 0 ? (
            <Text fz={12.5} c={C.muted}>
              No principals yet. Create one on the{" "}
              <Text component={Link} to="/principals" c={C.green} inherit>Principals</Text> tab,
              then mint its tokens here.
            </Text>
          ) : (
            <Group grow align="flex-start">
              <Select
                label="Principal"
                placeholder="trader / strategy / service"
                data={principalOpts}
                value={principalId}
                onChange={setPrincipalId}
                searchable
                styles={inputStyles}
              />
              <Select
                label="Grant portfolio (optional)"
                placeholder="no grant"
                data={portfolioOpts}
                value={portfolioId}
                onChange={setPortfolioId}
                clearable
                searchable
                styles={inputStyles}
              />
            </Group>
          )}
          <Group align="flex-end" justify="space-between">
            <TextInput
              label="Label (optional)"
              placeholder="e.g. prod-key-1"
              value={label}
              onChange={(e) => setLabel(e.currentTarget.value)}
              styles={inputStyles}
              style={{ flex: 1 }}
            />
            <Button color="depth" onClick={generate} loading={creating} disabled={!canGenerate}>
              Create token
            </Button>
          </Group>
        </Stack>
      </Box>

      {/* ── Reveal (terminal readout) ──────────────────────── */}
      {created && (
        <Box style={{ border: `1px solid ${C.border}`, borderLeft: `2px solid ${C.green}`, background: C.inset, borderRadius: 6, overflow: "hidden" }}>
          <Group justify="space-between" style={{ padding: "8px 16px", borderBottom: `1px solid ${C.border}` }}>
            <Eyebrow>New credential</Eyebrow>
            <Group gap={6}>
              <span style={{ width: 6, height: 6, borderRadius: "50%", background: C.amber }} />
              <Text fz={10} c={C.amber} style={{ letterSpacing: 1, textTransform: "uppercase" }}>
                shown once — copy now
              </Text>
            </Group>
          </Group>
          <Box style={{ paddingTop: 6, paddingBottom: 6 }}>
            <CmdLine
              value={created.token}
              display={<><Text component="span" c={C.faint} inherit>OMS_TOKEN=</Text>{created.token}</>}
            />
            <Box style={{ borderTop: `1px solid ${C.border}` }} />
            <CmdLine value={curl} display={curl} />
          </Box>
        </Box>
      )}

      {/* ── Ledger ─────────────────────────────────────────── */}
      <Box>
        <Group justify="space-between" mb={10}>
          <Eyebrow>Active tokens</Eyebrow>
          {tokens && <Text fz={11} c={C.faint}>{tokens.length}</Text>}
        </Group>
        <Box style={{ border: `1px solid ${C.border}`, borderRadius: 6, background: C.panel }}>
          {isLoading ? (
            <Group p={16}><Loader size="sm" color="depth" /></Group>
          ) : (tokens ?? []).length === 0 ? (
            <Text fz={13} c={C.faint} p={16}>No tokens yet. Create one above.</Text>
          ) : (
            <>
              <Group px={16} py={8} style={{ borderBottom: `1px solid ${C.border}` }} wrap="nowrap">
                <Box w={180}><Eyebrow>Label</Eyebrow></Box>
                <Box style={{ flex: 1 }}><Eyebrow>Principal</Eyebrow></Box>
                <Box w={230}><Eyebrow>Key id</Eyebrow></Box>
                <Box w={44} style={{ textAlign: "right" }}><Eyebrow>Age</Eyebrow></Box>
                <Box w={60} />
              </Group>
              {(tokens ?? []).map((t, i) => (
                <Group
                  key={t.key_id}
                  px={16}
                  py={10}
                  wrap="nowrap"
                  style={{ borderTop: i === 0 ? "none" : `1px solid ${C.border}` }}
                >
                  <Group gap={8} w={180} wrap="nowrap">
                    <span style={{ width: 6, height: 6, borderRadius: "50%", background: C.green, flex: "none" }} />
                    <Text fz={13} c={C.ink} truncate>
                      {t.label ?? <Text component="span" c={C.faint} inherit>unlabeled</Text>}
                    </Text>
                  </Group>
                  <Text fz={13} c={C.muted} style={{ flex: 1 }} truncate>
                    {t.principal_name ?? t.principal_code}
                  </Text>
                  <Box w={230}>
                    <Text
                      fz={11}
                      c={C.faint}
                      truncate
                      style={{ background: C.inset, border: `1px solid ${C.border}`, borderRadius: 4, padding: "2px 8px", display: "inline-block", maxWidth: "100%" }}
                    >
                      {t.key_id}
                    </Text>
                  </Box>
                  <Text fz={12} c={C.faint} w={44} ta="right">{ago(t.created_at)}</Text>
                  <Box w={60} style={{ textAlign: "right" }}>
                    <Text
                      fz={11}
                      c="#b0554f"
                      onClick={() => revoke(t.key_id)}
                      style={{ cursor: "pointer", letterSpacing: 0.5 }}
                    >
                      revoke
                    </Text>
                  </Box>
                </Group>
              ))}
            </>
          )}
        </Box>
      </Box>
    </Stack>
  );
}
