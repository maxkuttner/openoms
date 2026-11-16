import type { ReactNode } from "react";
import { Box, Group, Stack, Text } from "@mantine/core";
import { Logo } from "../components/Logo";
import { palette } from "../palette";

export const inputStyles = {
  input: { background: palette.surface, borderColor: palette.border, color: palette.ink, fontSize: 13 },
  label: { color: palette.muted, fontSize: 11, marginBottom: 4, letterSpacing: 0.3 },
};

/** The card every sign-in screen sits in. */
export function Frame({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Box style={{ minHeight: "100vh", background: palette.black, display: "grid", placeItems: "center", padding: 16 }}>
      <Box w={420} maw="100%" style={{ border: `1px solid ${palette.border}`, borderRadius: 8, background: palette.raised, overflow: "hidden" }}>
        {/* depth-ladder accent: green "bids" → amber "asks" */}
        <Box style={{ height: 2, background: `linear-gradient(90deg, ${palette.buy} 0%, ${palette.buy} 55%, ${palette.amber} 55%, ${palette.amber} 100%)` }} />
        <Stack gap={20} p={28}>
          <Group justify="space-between" align="center">
            <Logo size={20} />
          </Group>
          <Text fz={20} fw={800} c={palette.ink} style={{ letterSpacing: -0.4 }}>
            {title}
          </Text>
          {children}
        </Stack>
      </Box>
    </Box>
  );
}
