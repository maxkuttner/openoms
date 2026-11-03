// The venues a broker connection can point at, as `GET /admin/venues` describes
// them (src/venues.rs). The create form and the credentials drawer are built
// from that list, so adding a venue on the server adds it here. What a field
// means lives on the venue's docs page, so the drawer looks the same for all.

export type VenueField = {
  name: string;
  label: string;
  secret: boolean;
  input: "text" | "checkbox" | "textarea";
  // Connection settings (e.g. "FIX session") are grouped; key and secret are not.
  section: string | null;
  // Pre-fill for a PAPER / LIVE connection (e.g. a testnet host); never a secret.
  paper: string | null;
  live: string | null;
};

export type VenueEnvironment = { code: string; label: string; keys_url: string | null };

export type Venue = {
  code: string;
  name: string;
  description: string;
  docs_url: string;
  environments: VenueEnvironment[];
  fields: VenueField[];
};

type Option = { value: string; label: string };

export function connectionCode(venue: string, environment: string): string {
  return venue && environment ? `${venue}-${environment}`.toLowerCase() : "";
}

type CreateValues = { broker_code: string; environment: string; code: string };

// The code follows venue and environment while it is empty or still the one we
// filled in; once the user types their own, it is theirs.
export function autoCode(previous: CreateValues, next: CreateValues): string {
  const untouched = previous.code === "" || previous.code === connectionCode(previous.broker_code, previous.environment);
  return untouched ? connectionCode(next.broker_code, next.environment) : next.code;
}

export function venueOptions(venues: Venue[]): Option[] {
  return venues.map((v) => ({ value: v.code, label: `${v.name} — ${v.description}` }));
}

export function environmentOptions(venue: Venue | undefined): Option[] {
  if (!venue) return ["PAPER", "LIVE"].map((e) => ({ value: e, label: e }));
  return venue.environments.map((e) => ({ value: e.code, label: `${e.code} — ${e.label}` }));
}

// The credentials form's starting values: what is stored when the server
// echoes it, else (environment given, i.e. nothing stored yet) the venue's
// pre-fill for it. Secrets always start empty. With a stored credential pass
// null: a masked field must stay blank, or saving would overwrite it.
export function initialValues(
  fields: VenueField[],
  environment: string | null,
  stored: Map<string, { value: string | null }>,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const f of fields) {
    if (f.secret) {
      out[f.name] = "";
      continue;
    }
    const prefill = environment === null ? null : environment === "LIVE" ? f.live : f.paper;
    out[f.name] = stored.get(f.name)?.value ?? prefill ?? (f.input === "checkbox" ? "true" : "");
  }
  return out;
}

// The drawer's layout: credentials on top, then each settings section.
export function groupFields(fields: VenueField[]): {
  main: VenueField[];
  sections: { name: string; fields: VenueField[] }[];
} {
  const sections: { name: string; fields: VenueField[] }[] = [];
  for (const f of fields.filter((f) => f.section)) {
    const section = sections.find((s) => s.name === f.section);
    if (section) section.fields.push(f);
    else sections.push({ name: f.section!, fields: [f] });
  }
  return { main: fields.filter((f) => !f.section), sections };
}

// Closed when it is all filled in (e.g. the testnet pre-fill), open when
// something in it still needs typing.
export function sectionStartsOpen(fields: VenueField[], values: Record<string, string>): boolean {
  return fields.some((f) => f.input !== "checkbox" && !(values[f.name] ?? "").trim());
}
