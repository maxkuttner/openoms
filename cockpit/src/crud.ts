// Form values for CrudResource's create/edit modal, kept apart from the
// component so they can be tested without React.

type FieldLike = {
  name: string;
  type?: string;
  default?: string;
  required?: boolean;
  inCreate?: boolean;
  inEdit?: boolean;
  nullable?: boolean;
};

// Editing starts from the row; a new row from each field's default, else empty.
export function initialFormValues(fields: FieldLike[], row: Record<string, any> | null): Record<string, any> {
  const out: Record<string, any> = {};
  for (const f of fields) {
    const empty = f.type === "checkbox" ? false : "";
    out[f.name] = (row ? row[f.name] : f.default) ?? empty;
  }
  return out;
}

// The JSON sent on save. Empty strings are dropped, so an optional field isn't
// sent as "" and the server keeps (edit) or defaults (create) it. A `nullable`
// field emptied on edit is sent as null instead, which the server reads as
// "clear it".
export function requestBody(vals: Record<string, any>, fields: FieldLike[], isCreate: boolean): Record<string, any> {
  const out: Record<string, any> = {};
  for (const f of fields) {
    if (isCreate ? f.inCreate === false : f.inEdit === false) continue;
    const v = vals[f.name];
    if (v === "" || v === undefined || v === null) {
      if (isCreate && f.required) out[f.name] = v;
      else if (!isCreate && f.nullable) out[f.name] = null;
      continue;
    }
    out[f.name] = v;
  }
  return out;
}
