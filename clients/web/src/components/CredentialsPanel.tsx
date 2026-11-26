import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Textarea } from "@/components/ui/textarea";
import { FormRow, Notice, Spinner } from "./common";
import { ConfirmButton } from "./ConfirmButton";
import { TextField } from "./form";
import { Modal } from "./overlay";
import { Pill, type Tone } from "./status";
import { api, ApiError } from "../api/client";
import { notifyError, notifyOk, notifyWarn, useList } from "../api/hooks";
import { describeSaveTest, describeTest } from "../credentialTest";
import { groupFields, initialValues, missingFields, sectionStartsOpen, type Venue, type VenueField } from "../venues";
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


const STATE_TONE: Record<RedactedCredentials["state"], Tone> = {
  configured: "buy",
  unconfigured: "muted",
  error: "sell",
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

  const [values, setValues] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const setField = (name: string, value: string) => {
    setValues((v) => ({ ...v, [name]: value }));
    setErrors((e) => ({ ...e, [name]: "" }));
  };
  // `required` on an input only renders an asterisk; enforcement is this
  // validator. It fires only when there is nothing stored to fall back on —
  // once a credential is configured, an empty field means "keep", which is
  // the whole point of the merge rule and must not be flagged as missing.
  const validate = (vals: Record<string, string>) => missingFields(specs, vals, isFreshRef.current);
  const [lastTest, setLastTest] = useState<TestResponse | null>(null);
  const [lastSave, setLastSave] = useState<SaveResponse | null>(null);

  // `configured` is the only state where the server hands back real values —
  // `unconfigured` and (per parse_broker/parse_feed) `error` are both parsed
  // with `existing: None`, so both need every field filled in fresh.
  const isFresh = data?.state !== "configured";
  // The validator above is declared before `isFresh` is in scope — a ref
  // keeps it reading the current value.
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
    setValues(init);
    setErrors({});
    setLastTest(null);
    setLastSave(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [code, providerCode, environment, specs.length, data?.state, JSON.stringify(data?.fields)]);

  const testMut = useMutation({
    mutationFn: () => api.post<TestResponse>(testPath, undefined),
    onSuccess: (resp) => {
      setLastTest(resp);
      if (resp.tested && resp.warning) notifyWarn(resp.message ?? "The venue is not ready yet");
      else if (resp.tested && !resp.ok) notifyError(resp.message ?? "credential test failed");
      else if (resp.tested) notifyOk(resp.message ?? "Credential test passed");
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
      const note = describeSaveTest(resp);
      const text = `Saved. ${note.text} ${describeReload(resp.reload)}.`;
      if (note.tone === "warning") notifyWarn(text);
      else notifyOk(text);
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

  if (isLoading || venues.isLoading) return <Spinner />;
  if (isError || !data) {
    return <Notice>Failed to load credentials{error instanceof ApiError ? `: ${error.message}` : ""}</Notice>;
  }
  if (specs.length === 0) {
    return <Notice>No credential fields known for {kind} "{providerCode}". For a broker, add it to src/venues.rs.</Notice>;
  }

  const renderField = (spec: VenueField) => {
    if (spec.input === "checkbox") {
      return (
        <label key={spec.name} className="flex items-center gap-2">
          <Checkbox checked={values[spec.name] === "true"} onCheckedChange={(on) => setField(spec.name, on === true ? "true" : "false")} />
          {spec.label}
        </label>
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
    const label = <>{spec.label}{isFresh && <span className="text-sell"> *</span>}</>;
    if (spec.input === "textarea") {
      return (
        <FormRow key={spec.name} label={label} error={errors[spec.name]}>
          <Textarea className="num min-h-20 text-xs" placeholder={placeholder} value={values[spec.name] ?? ""}
            onChange={(e) => setField(spec.name, e.currentTarget.value)} aria-invalid={!!errors[spec.name]} />
        </FormRow>
      );
    }
    return (
      <TextField key={spec.name} label={spec.label} required={isFresh && !spec.optional} type={spec.secret ? "password" : "text"}
        placeholder={spec.secret ? placeholder : undefined} value={values[spec.name] ?? ""}
        onChange={(v) => setField(spec.name, v)} error={errors[spec.name]} autoComplete="off" />
    );
  };

  const canTest = data.state === "configured";
  const canClear = data.state !== "unconfigured";

  const submit = () => {
    const errs = validate(values);
    setErrors(errs);
    if (Object.keys(errs).length === 0) saveMut.mutate(values);
  };
  const toggleSection = (name: string, open: boolean) =>
    setOpenSections((s) => (open ? [...new Set([...s, name])] : s.filter((x) => x !== name)));

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        <Pill tone={STATE_TONE[data.state]}>{data.state}</Pill>
        {data.updated_at && (
          <span className="num text-xs text-muted-foreground">credentials last saved {new Date(data.updated_at).toLocaleString()}</span>
        )}
      </div>

      {venue && (
        <p>
          <a href={venue.docs_url} target="_blank" rel="noreferrer" className="text-primary hover:underline">Setup guide ↗</a>
          {keysUrl && (
            <>
              {" · "}
              <a href={keysUrl} target="_blank" rel="noreferrer" className="text-primary hover:underline">{venue.name} API keys ↗</a>
            </>
          )}
        </p>
      )}

      {data.state === "error" && <Notice title="Stored credential is unusable">{data.message ?? "unknown error"}</Notice>}

      <form onSubmit={(e) => { e.preventDefault(); submit(); }} className="flex flex-col gap-2.5">
        {layout.main.map(renderField)}
        {layout.sections.map((sec) => (
          <details key={sec.name} open={openSections.includes(sec.name)} className="group rounded-md border"
            onToggle={(e) => toggleSection(sec.name, e.currentTarget.open)}>
            <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 font-medium">
              <ChevronRight className="size-3.5 transition-transform group-open:rotate-90" />
              {sec.name}
            </summary>
            <div className="flex flex-col gap-2.5 px-3 pb-3">{sec.fields.map(renderField)}</div>
          </details>
        ))}

        <div className="flex items-center justify-between">
          <div className="flex gap-2">
            {canTest && (
              <Button type="button" variant="secondary" disabled={testMut.isPending} onClick={() => testMut.mutate()}>Test</Button>
            )}
            {canClear && (
              <ConfirmButton variant="destructive" disabled={clearMut.isPending} title={`Clear the stored credential for ${code}?`}
                description="The connection will be disarmed." confirmLabel="Clear" onConfirm={() => clearMut.mutate()}>
                Clear
              </ConfirmButton>
            )}
          </div>
          <Button type="submit" disabled={saveMut.isPending}>Save</Button>
        </div>
      </form>

      {lastTest && (
        <Notice tone={describeTest(lastTest).tone} title="Test result">
          {describeTest(lastTest).text}
        </Notice>
      )}

      {lastSave && (
        <Notice tone={describeSaveTest(lastSave).tone} title="Save result">
          {describeSaveTest(lastSave).text} {describeReload(lastSave.reload)}.
        </Notice>
      )}

      {venue?.universe && (
        <Modal open={offerLoad} onClose={() => setOfferLoad(false)} title="Credentials saved ✓"
          description={`${venue.universe} — runs in the background.`}
          footer={<>
            <Button variant="outline" onClick={() => setOfferLoad(false)}>Later</Button>
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
          </>}>
          <p>Load {venue.name}'s instrument universe now?</p>
        </Modal>
      )}
    </div>
  );
}
