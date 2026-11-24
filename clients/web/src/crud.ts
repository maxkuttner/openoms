// Form values for CrudResource's create/edit modal, kept apart from the
// component so they can be tested without React.

type FieldLike = { name: string; type?: string; default?: string };

// Editing starts from the row; a new row from each field's default, else empty.
export function initialFormValues(fields: FieldLike[], row: Record<string, any> | null): Record<string, any> {
  const out: Record<string, any> = {};
  for (const f of fields) {
    const empty = f.type === "checkbox" ? false : "";
    out[f.name] = (row ? row[f.name] : f.default) ?? empty;
  }
  return out;
}
