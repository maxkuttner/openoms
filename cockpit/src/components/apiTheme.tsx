import { Box, CopyButton, Group, Text, Tooltip } from "@mantine/core";

// Shared terminal / "Databento" visual tokens + primitives, used by the API
// tokens page and the API reference. Green "bids", amber "asks", near-black ink.
export const C = {
  panel: "#1a1e26",
  inset: "#0b0d10",
  border: "#2a2f38",
  ink: "#eff1f4",
  muted: "#9aa3af",
  faint: "#6b7280",
  green: "#22ae6c",
  amber: "#ce9a3b",
  red: "#b0554f",
};

export function Eyebrow({ children }: { children: React.ReactNode }) {
  return (
    <Text fz={10} c={C.faint} style={{ letterSpacing: 2, textTransform: "uppercase" }}>
      {children}
    </Text>
  );
}

/** A single `$`-prompted terminal line with an inline copy affordance. */
export function CmdLine({ value, display }: { value: string; display: React.ReactNode }) {
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
