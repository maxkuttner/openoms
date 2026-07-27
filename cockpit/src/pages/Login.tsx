import { useState } from "react";
import { Box, Button, Group, PasswordInput, Stack, Text, TextInput } from "@mantine/core";
import { Logo } from "../components/Logo";

// Brand tokens (theme.ts): green "bids", amber "asks", near-black terminal ink.
const C = {
  bg: "#0d1014",
  panel: "#1a1e26",
  inset: "#0b0d10",
  border: "#2a2f38",
  ink: "#eff1f4",
  muted: "#9aa3af",
  faint: "#6b7280",
  green: "#22ae6c",
  amber: "#ce9a3b",
};

/**
 * Console sign-in. The username is cosmetic (single shared admin); the password is
 * the OMS admin password, sent as a bearer to /admin. `onSubmit` gets the password;
 * the caller verifies it.
 */
export function LoginPage({
  onSubmit,
  error,
  checking,
}: {
  onSubmit: (password: string) => void;
  error?: string;
  checking?: boolean;
}) {
  const [username, setUsername] = useState("admin");
  const [password, setPassword] = useState("");

  const inputStyles = {
    input: { background: C.inset, borderColor: C.border, color: C.ink, fontSize: 13 },
    label: { color: C.muted, fontSize: 11, marginBottom: 4, letterSpacing: 0.3 },
  };

  return (
    <Box
      style={{
        minHeight: "100vh",
        background: C.bg,
        display: "grid",
        placeItems: "center",
        padding: 16,
      }}
    >
      <Box w={380} style={{ border: `1px solid ${C.border}`, borderRadius: 8, background: C.panel, overflow: "hidden" }}>
        {/* depth-ladder accent: green "bids" → amber "asks" */}
        <Box style={{ height: 2, background: `linear-gradient(90deg, ${C.green} 0%, ${C.green} 55%, ${C.amber} 55%, ${C.amber} 100%)` }} />
        <Stack gap={20} p={28}>
          <Group justify="space-between" align="center">
            <Logo size={20} />
            <Text fz={10} c={C.faint} style={{ letterSpacing: 2, textTransform: "uppercase" }}>
              Console
            </Text>
          </Group>

          <div>
            <Text fz={20} fw={800} c={C.ink} style={{ letterSpacing: -0.4 }}>
              Sign in
            </Text>
            <Text fz={12.5} c={C.muted} mt={4}>
              Enter your admin password to open the console.
            </Text>
          </div>

          <form
            onSubmit={(e) => {
              e.preventDefault();
              onSubmit(password);
            }}
          >
            <Stack gap={12}>
              <TextInput
                label="Username"
                value={username}
                onChange={(e) => setUsername(e.currentTarget.value)}
                autoComplete="username"
                styles={inputStyles}
              />
              <PasswordInput
                label="Password"
                placeholder="admin password"
                value={password}
                onChange={(e) => setPassword(e.currentTarget.value)}
                error={error}
                autoComplete="current-password"
                data-autofocus
                styles={inputStyles}
              />
              <Button type="submit" color="depth" loading={checking} fullWidth mt={4}>
                Sign in
              </Button>
            </Stack>
          </form>

          <Text fz={11} c={C.faint} ta="center">
            Set by <Text component="span" c={C.muted} inherit>OMS_ADMIN_PASSWORD</Text>
          </Text>
        </Stack>
      </Box>
    </Box>
  );
}
