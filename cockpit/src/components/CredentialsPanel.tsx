import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useForm } from "@mantine/form";
import {
  Alert,
  Badge,
  Button,
  Checkbox,
  Group,
  Loader,
  PasswordInput,
  Stack,
  Text,
  TextInput,
  Textarea,
} from "@mantine/core";
import { api, ApiError } from "../api/client";
import { notifyError, notifyOk } from "../api/hooks";
import type { ConnectionOutcome, RedactedCredentials, SaveResponse, TestResponse } from "../api/types";

export type CredentialsKind = "broker" | "feed";

type Props = {
  kind: CredentialsKind;
  /** The connection's own `code` (its AES-GCM AAD — never editable here). */
  code: string;
  /** `broker_code` for a broker connection, `provider` for a feed connection —
   * selects which field list applies. */
  providerCode: string;
};

// ---------------------------------------------------------------------------
// Field map — THE one piece of duplicated knowledge in this design.
//
// The server only tells the cockpit the field list when a credential is
// already `configured` (`RedactedCredentials.fields`). When it is
// `unconfigured` (or `error`, which is parsed exactly like unconfigured — see
// `parse_broker`/`parse_feed`'s `existing: None` case in
// src/credentials_api.rs), the server sends an empty `fields` array and the
// form has nothing to build itself from. So the client must know, per
// broker/feed kind, which fields exist and which are secret.
//
// This must be kept in sync BY HAND with:
//   - src/credentials.rs: `BrokerCredentials` / `FeedCredentials` enum
//     variants (the field names and which exist at all)
//   - src/credentials.rs: `Redact for BrokerCredentials` / `FeedCredentials`
//     (`shown(...)` = not secret, `hidden(...)` = secret)
//   - src/credentials_api.rs: `parse_broker` / `parse_feed` (the `kind`
//     strings — "ALPACA" / "IBKR" / "BINANCE" / "DATABENTO" — and which
//     fields are required)
//
// Adding a broker or feed provider means adding an entry here too.
// ---------------------------------------------------------------------------

type FieldSpec = {
  name: string;
  label: string;
  secret: boolean;
  input: "text" | "checkbox" | "textarea";
};

const BROKER_FIELDS: Record<string, FieldSpec[]> = {
  ALPACA: [
    { name: "key", label: "Key ID", secret: false, input: "text" },
    { name: "secret", label: "Secret", secret: true, input: "text" },
  ],
  IBKR: [
    { name: "host", label: "Host", secret: false, input: "text" },
    { name: "port", label: "Port", secret: false, input: "text" },
    { name: "sender_comp_id", label: "Sender comp ID", secret: false, input: "text" },
    { name: "target_comp_id", label: "Target comp ID", secret: false, input: "text" },
    { name: "ssl", label: "SSL", secret: false, input: "checkbox" },
    { name: "password", label: "Password", secret: true, input: "text" },
  ],
  BINANCE: [
    { name: "host", label: "Host", secret: false, input: "text" },
    { name: "port", label: "Port", secret: false, input: "text" },
    { name: "sender_comp_id", label: "Sender comp ID", secret: false, input: "text" },
    { name: "target_comp_id", label: "Target comp ID", secret: false, input: "text" },
    { name: "api_key", label: "API key", secret: true, input: "text" },
    { name: "private_key", label: "Private key (PEM)", secret: true, input: "textarea" },
  ],
};

const FEED_FIELDS: Record<string, FieldSpec[]> = {
  DATABENTO: [{ name: "api_key", label: "API key", secret: true, input: "text" }],
};

function fieldSpecsFor(kind: CredentialsKind, providerCode: string): FieldSpec[] {
  return (kind === "broker" ? BROKER_FIELDS : FEED_FIELDS)[providerCode] ?? [];
}

// ---------------------------------------------------------------------------

function basePathFor(kind: CredentialsKind): string {
  return kind === "broker" ? "/admin/broker-connections" : "/admin/feed-connections";
}

/** `Reload outcome -> what to tell the operator, per the addendum's mapping. */
function describeReload(outcome: ConnectionOutcome | null): string {
  if (outcome === null) return "reload status unknown";
  if (outcome === "Registered") return "applied immediately";
  if (outcome === "RestartRequired") return "saved — a process restart is needed for this to take effect";
  if (outcome === "Unconfigured") return "saved, but the connection now reports unconfigured";
  if (outcome === "Disabled") return "saved, but the connection is disabled (status is not ACTIVE)";
  return `saved, but the connection did not come up: ${outcome.Failed}`;
}

function describeTest(tested: boolean, ok: boolean, message: string | null): { color: string; text: string } {
  if (!tested) return { color: "gray", text: message ?? "not testable before save" };
  if (ok) return { color: "green", text: "credential verified" };
  return { color: "red", text: message ?? "credential test failed" };
}

const STATE_COLOR: Record<RedactedCredentials["state"], string> = {
  configured: "green",
  unconfigured: "gray",
  error: "red",
};

export function CredentialsPanel({ kind, code, providerCode }: Props) {
  const basePath = basePathFor(kind);
  const credPath = `${basePath}/${code}/credentials`;
  const testPath = `${credPath}/test`;
  const specs = fieldSpecsFor(kind, providerCode);
  const qc = useQueryClient();

  const { data, isLoading, isError, error } = useQuery<RedactedCredentials>({
    queryKey: [credPath],
    queryFn: () => api.get<RedactedCredentials>(credPath),
  });

  // `required` on an input only renders an asterisk; enforcement is this
  // validator. It fires only when there is nothing stored to fall back on —
  // once a credential is configured, an empty field means "keep", which is
  // the whole point of the merge rule and must not be flagged as missing.
  const form = useForm<Record<string, string>>({
    initialValues: {},
    validate: (values) =>
      Object.fromEntries(
        specs
          .filter((spec) => spec.input !== "checkbox")
          .map((spec) => [
            spec.name,
            isFreshRef.current && !(values[spec.name] ?? "").trim() ? "required" : null,
          ]),
      ),
  });
  const [lastTest, setLastTest] = useState<TestResponse | null>(null);
  const [lastSave, setLastSave] = useState<SaveResponse | null>(null);

  // `configured` is the only state where the server hands back real values —
  // `unconfigured` and (per parse_broker/parse_feed) `error` are both parsed
  // with `existing: None`, so both need every field filled in fresh.
  const isFresh = data?.state !== "configured";
  // `form` is created before `isFresh` is in scope, and Mantine captures the
  // validator once — a ref keeps it reading the current value.
  const isFreshRef = useRef(isFresh);
  isFreshRef.current = isFresh;

  // The server's fields by name, used both to seed the form and to render
  // each input's placeholder.
  const wireByName = useMemo(
    () => new Map((data?.fields ?? []).map((f) => [f.name, f])),
    [data?.fields],
  );

  useEffect(() => {
    const init: Record<string, string> = {};
    for (const spec of specs) {
      if (spec.secret) {
        init[spec.name] = ""; // never prefilled — the server never sends a secret value
        continue;
      }
      // Only a non-null `value` is prefillable. A masked identifier (Alpaca's
      // key id) arrives with `value: null` and a `hint`, precisely so it is
      // not prefilled — the merge rule reads any non-empty submission as a
      // replacement, so a prefilled mask would overwrite the real value the
      // first time the operator edited some other field.
      const wire = wireByName.get(spec.name);
      init[spec.name] = wire?.value ?? (spec.input === "checkbox" ? "true" : "");
    }
    form.setValues(init);
    setLastTest(null);
    setLastSave(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [code, providerCode, data?.state, JSON.stringify(data?.fields)]);

  const testMut = useMutation({
    mutationFn: () => api.post<TestResponse>(testPath, undefined),
    onSuccess: (resp) => {
      setLastTest(resp);
      if (resp.tested && !resp.ok) notifyError(resp.message ?? "credential test failed");
      else if (resp.tested) notifyOk("Credential test passed");
      // tested === false: no toast — the inline banner below says why.
    },
    onError: notifyError,
  });

  const saveMut = useMutation({
    mutationFn: (vals: Record<string, string>) => api.put<SaveResponse>(credPath, vals),
    onSuccess: (resp) => {
      setLastSave(resp);
      setLastTest(null);
      qc.invalidateQueries({ queryKey: [credPath] });
      const testNote = resp.tested ? "Credential verified." : `Not tested before save (${resp.message}).`;
      notifyOk(`Saved. ${testNote} ${describeReload(resp.reload)}.`);
    },
    onError: notifyError,
  });

  const clearMut = useMutation({
    mutationFn: () => api.del(credPath),
    onSuccess: () => {
      setLastTest(null);
      setLastSave(null);
      qc.invalidateQueries({ queryKey: [credPath] });
      notifyOk("Credential cleared");
    },
    onError: notifyError,
  });

  if (isLoading) return <Loader size="sm" />;
  if (isError || !data) {
    return <Alert color="red">Failed to load credentials{error instanceof ApiError ? `: ${error.message}` : ""}</Alert>;
  }
  if (specs.length === 0) {
    return (
      <Alert color="red">
        No field definition for {kind} kind "{providerCode}" — add one to CredentialsPanel.tsx's field map.
      </Alert>
    );
  }

  const canTest = data.state === "configured";
  const canClear = data.state !== "unconfigured";

  return (
    <Stack gap="sm">
      <Group gap="xs">
        <Badge color={STATE_COLOR[data.state]}>{data.state}</Badge>
        {data.updated_at && (
          <Text size="xs" c="dimmed">
            credentials last saved {new Date(data.updated_at).toLocaleString()}
          </Text>
        )}
      </Group>

      {data.state === "error" && (
        <Alert color="red" title="Stored credential is unusable">
          {data.message ?? "unknown error"}
        </Alert>
      )}

      <form
        onSubmit={form.onSubmit((vals) => {
          saveMut.mutate(vals);
        })}
      >
        <Stack gap="xs">
          {specs.map((spec) => {
            const common = {
              label: spec.label,
              required: isFresh,
              ...form.getInputProps(spec.name),
            };
            if (spec.input === "checkbox") {
              return (
                <Checkbox
                  key={spec.name}
                  label={spec.label}
                  checked={form.values[spec.name] === "true"}
                  onChange={(e) => form.setFieldValue(spec.name, e.currentTarget.checked ? "true" : "false")}
                />
              );
            }
            // A withheld field — secret or masked — shows what is installed
            // (where there is a hint) and says blank means keep.
            const hint = wireByName.get(spec.name)?.hint ?? null;
            const placeholder = isFresh
              ? undefined
              : hint
                ? `${hint} — leave blank to keep`
                : spec.secret
                  ? "leave blank to keep"
                  : undefined;
            if (spec.input === "textarea") {
              return (
                <Textarea
                  key={spec.name}
                  {...common}
                  placeholder={placeholder}
                  autosize
                  minRows={3}
                  styles={{ input: { fontFamily: "monospace" } }}
                />
              );
            }
            if (spec.secret) {
              return <PasswordInput key={spec.name} {...common} placeholder={placeholder} />;
            }
            return <TextInput key={spec.name} {...common} />;
          })}

          <Group justify="space-between">
            <Group gap="xs">
              {canTest && (
                <Button
                  type="button"
                  variant="light"
                  loading={testMut.isPending}
                  onClick={() => testMut.mutate()}
                >
                  Test
                </Button>
              )}
              {canClear && (
                <Button
                  type="button"
                  variant="light"
                  color="red"
                  loading={clearMut.isPending}
                  onClick={() => {
                    if (confirm(`Clear the stored credential for ${code}? The connection will be disarmed.`)) {
                      clearMut.mutate();
                    }
                  }}
                >
                  Clear
                </Button>
              )}
            </Group>
            <Button type="submit" loading={saveMut.isPending}>
              Save
            </Button>
          </Group>
        </Stack>
      </form>

      {lastTest && (
        <Alert color={describeTest(lastTest.tested, lastTest.ok, lastTest.message).color} title="Test result">
          {describeTest(lastTest.tested, lastTest.ok, lastTest.message).text}
        </Alert>
      )}

      {lastSave && (
        <Alert color="blue" title="Save result">
          {lastSave.tested ? "Credential verified. " : `Not tested before save (${lastSave.message}). `}
          {describeReload(lastSave.reload)}.
        </Alert>
      )}
    </Stack>
  );
}
