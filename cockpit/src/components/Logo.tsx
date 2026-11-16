// The brand depth-ladder glyph: green "bids" over amber "asks".
export function DepthGlyph({ height = 24 }: { height?: number }) {
  const w = (height * 26) / 24;
  return (
    <svg width={w} height={height} viewBox="0 0 26 24" aria-hidden role="presentation">
      <rect x="0" y="0" width="15.6" height="3" rx="1" fill="var(--buy)" />
      <rect x="0" y="5.5" width="22.1" height="3" rx="1" fill="var(--buy)" />
      <rect x="0" y="11" width="26" height="2" rx="1" fill="var(--border)" />
      <rect x="0" y="15" width="18.7" height="3" rx="1" fill="var(--primary)" />
      <rect x="0" y="20.5" width="23.4" height="3" rx="1" fill="var(--primary)" />
    </svg>
  );
}

// "openOMS" wordmark — muted "open", bold "OMS".
export function Logo({ size = 15 }: { size?: number }) {
  return (
    <span className="inline-flex items-center gap-2 whitespace-nowrap">
      <DepthGlyph height={size + 2} />
      <span style={{ fontSize: size, letterSpacing: -0.5, lineHeight: 1 }}>
        <span className="font-normal text-muted-foreground">open</span>
        <span className="font-semibold text-foreground">OMS</span>
      </span>
    </span>
  );
}
