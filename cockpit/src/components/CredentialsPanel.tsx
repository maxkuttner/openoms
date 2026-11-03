import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useForm } from "@mantine/form";
import {
  Accordion,
  Alert,
  Anchor,
  Badge,
  Button,
  Checkbox,
  Group,
  Loader,
  Modal,
  PasswordInput,
  Stack,
  Text,
  TextInput,
  Textarea,
} from "@mantine/core";
import { api, ApiError } from "../api/client";
import { notifyError, notifyOk, useList } from "../api/hooks";
import { groupFields, initialValues, sectionStartsOpen, type Venue, type VenueField } from "../venues";
import type { ConnectionOutcome, RedactedCredentials, SaveResponse, TestResponse } from "../api/types";
import type { UniverseStatus } from "../api/jobs";
import { loadUniverse } from "./UniverseStatus";

export type CredentialsKind = "broker" | "feed";


type Props = {
  kind: CredentialsKind;
  /** The connection's own `code` (its AES-GCM AAD — never editable here). */
  code: string;
  /** `broker_code` for a broker connection, `provider` for a feed connection —
   * selects which field list applies. */
  providerCode: string;
  /** A broker connection's environment: picks the venue's pre-fill and keys page. */
  environment?: string;
};

// Which fields each credential has and which are secret. A broker
// connection's come from the server's venue list (`GET /admin/venues`,
// src/venues.rs), whose tests hold them to `BrokerCredentials`; what each one
// means is on the venue's docs page, linked at the top of the drawer.
// Feeds are not in that list yet, so their one entry lives here and must match
// src/credentials.rs's `FeedCredentials` and src/credentials_api.rs's
// `parse_feed` by hand.
const FEED_FIELDS: Record<string, VenueField[]> = {
  DATABENTO: [
    {
      name: "api_key",
      label: "API key",
      secret: true,
      input: "text",
      section: null,
      paper: null,
      live: null,
    },
  ],
};

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

export function CredentialsPanel({ kind, code, providerCode, environment = "PAPER" }: Props) {
  const basePath = basePathFor(kind);
  const credPath = `${basePath}/${code}/credentials`;
  const testPath = `${credPath}/test`;
  const venues = useList<Venue>("/admin/venues", kind === "broker");
  const venue = venues.data?.find((v) => v.code === providerCode);
  const specs = (kind === "broker" ? venue?.fields : FEED_FIELDS[providerCode]) ?? [];
  const keysUrl = venue?.environments.find((e) => e.code === environment)?.keys_url ?? null;
  const layout = groupFields(specs);
  const [openSections, setOpenSections] = useState<string[]>([]);
  const qc = useQueryClient();

  const { data, isLoading, isError, error } = useQuery<RedactedCredentials>({
    queryKey: [credPath],
    queryFn: () => api.get<RedactedCredentials>(credPath),
  });

  // Offered once, right after a broker credential is saved, when that
  // connection's instrument universe was never loaded.
  const [offerLoad, setOfferLoad] = useState(false);

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
    // Only a non-null `value` is prefillable. A masked identifier (Alpaca's
    // key id) arrives with `value: null` and a `hint`, precisely so it is not
    // prefilled — the merge rule reads any non-empty submission as a
    // replacement, so a prefilled mask would overwrite the real value the
    // first time the operator edited some other field. The venue's pre-fill
    // applies only while nothing is stored, for the same reason.
    const init = initialValues(specs, isFresh ? environment : null, wireByName);
    setOpenSections(layout.sections.filter((sec) => sectionStartsOpen(sec.fields, init)).map((sec) => sec.name));
    form.setValues(init);
    setLastTest(null);
    setLastSave(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [code, providerCode, environment, specs.length, data?.state, JSON.stringify(data?.fields)]);

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
      if (kind === "broker" && venue?.universe) {
        api
          .get<UniverseStatus>(`${basePath}/${code}/universe`)
          .then((u) => setOfferLoad(u.state === "not_loaded"))
          .catch(() => {});
      }
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

  if (isLoading || venues.isLoading) return <Loader size="sm" />;
  if (isError || !data) {
    return <Alert color="red">Failed to load credentials{error instanceof ApiError ? `: ${error.message}` : ""}</Alert>;
  }
  if (specs.length === 0) {
    return (
      <Alert color="red">
        No credential fields known for {kind} "{providerCode}". For a broker, add it to src/venues.rs.
      </Alert>
    );
  }

  const renderField = (spec: VenueField) => {
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
  };

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

      {venue && (
        <Text size="sm">
          <Anchor href={venue.docs_url} target="_blank" rel="noreferrer">
            Setup guide ↗
          </Anchor>
          {keysUrl && (
            <>
              {" · "}
              <Anchor href={keysUrl} target="_blank" rel="noreferrer">
                {venue.name} API keys ↗
              </Anchor>
            </>
          )}
        </Text>
      )}

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
          {layout.main.map(renderField)}
          {layout.sections.length > 0 && (
            <Accordion multiple variant="contained" value={openSections} onChange={setOpenSections}>
              {layout.sections.map((sec) => (
                <Accordion.Item key={sec.name} value={sec.name}>
                  <Accordion.Control>{sec.name}</Accordion.Control>
                  <Accordion.Panel>
                    <Stack gap="xs">{sec.fields.map(renderField)}</Stack>
                  </Accordion.Panel>
                </Accordion.Item>
              ))}
            </Accordion>
          )}

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
        <Alert color="brand" title="Save result">
          {lastSave.tested ? "Credential verified. " : `Not tested before save (${lastSave.message}). `}
          {describeReload(lastSave.reload)}.
        </Alert>
      )}

      {venue?.universe && (
        <Modal opened={offerLoad} onClose={() => setOfferLoad(false)} title="Credentials saved ✓" centered>
          <Stack>
            <Text>Load {venue.name}'s instrument universe now?</Text>
            <Text fz="sm" c="dimmed">
              {venue.universe} — runs in the background.
            </Text>
            <Group justify="flex-end">
              <Button variant="default" onClick={() => setOfferLoad(false)}>
                Later
              </Button>
              <Button
                onClick={async () => {
                  setOfferLoad(false);
                  if (await loadUniverse(code)) {
                    qc.invalidateQueries({ queryKey: ["/admin/broker-connections", code, "universe"] });
                    qc.invalidateQueries({ queryKey: ["/admin/jobs"] });
                  }
                }}
              >
                Load now
              </Button>
            </Group>
          </Stack>
        </Modal>
      )}
    </Stack>
  );
}
