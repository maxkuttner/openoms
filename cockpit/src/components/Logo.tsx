import { Group, Text } from "@mantine/core";

// The brand depth-ladder glyph: green "bids" over amber "asks".
export function DepthGlyph({ height = 24 }: { height?: number }) {
  const w = (height * 26) / 24;
  return (
    <svg width={w} height={height} viewBox="0 0 26 24" aria-hidden role="presentation">
      <rect x="0" y="0" width="15.6" height="3" rx="1" fill="#25b083" />
      <rect x="0" y="5.5" width="22.1" height="3" rx="1" fill="#25b083" />
      <rect x="0" y="11" width="26" height="2" rx="1" fill="#2a2f38" />
      <rect x="0" y="15" width="18.7" height="3" rx="1" fill="#ce9a3b" />
      <rect x="0" y="20.5" width="23.4" height="3" rx="1" fill="#ce9a3b" />
    </svg>
  );
}

// "openOMS" wordmark — muted "open", bold "OMS".
export function Logo({ size = 22 }: { size?: number }) {
  return (
    <Group gap={10} wrap="nowrap">
      <DepthGlyph height={size + 2} />
      <Text component="span" fz={size} style={{ letterSpacing: -1, lineHeight: 1 }}>
        <Text component="span" fw={400} c="#6b7280" inherit>
          open
        </Text>
        <Text component="span" fw={800} c="#eff1f4" inherit>
          OMS
        </Text>
      </Text>
    </Group>
  );
}
