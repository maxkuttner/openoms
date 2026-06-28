import { useState, type ReactNode } from "react";
import {
  Group, Title, Button, Table, Modal, Stack, TextInput, NumberInput,
  Select, Checkbox, ActionIcon, Text, Loader, Badge,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { api } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import { InstrumentSelect } from "./InstrumentSelect";

export type Field = {
  name: string;
  label: string;
  type?: "text" | "number" | "select" | "checkbox" | "instrument";
  options?: { value: string; label: string }[];
  // Populate a select from another list endpoint, mapping each row to {value,label}.
  optionsPath?: string;
  optionValue?: string;
  optionLabel?: string;
  required?: boolean;
  inCreate?: boolean; // default true
  inEdit?: boolean; // default true
};

export type Column = { key: string; label: string; render?: (row: any) => ReactNode };

type Props = {
  title: string;
  path: string; // collection path, e.g. "/admin/accounts"
  idKey?: string; // default "id"
  columns: Column[];
  fields: Field[];
  editable?: boolean;
  deletable?: boolean;
};

// Render the value of one field, resolving select options (incl. async optionsPath).
function FieldInput({ field, form }: { field: Field; form: any }) {
  const remote = useList<any>(field.optionsPath ?? "", !!field.optionsPath);
  const props = { label: field.label, ...form.getInputProps(field.name) };
  switch (field.type) {
    case "instrument":
      return (
        <InstrumentSelect
          label={field.label}
          required={field.required}
          value={(form.values[field.name] as string) || null}
          onChange={(v) => form.setFieldValue(field.name, v ?? "")}
        />
      );
    case "number":
      return <NumberInput {...props} />;
    case "checkbox":
      return <Checkbox label={field.label} {...form.getInputProps(field.name, { type: "checkbox" })} />;
    case "select": {
      const opts =
        field.options ??
        (remote.data ?? []).map((r) => ({
          value: String(r[field.optionValue ?? "id"]),
          label: String(r[field.optionLabel ?? "code"]),
        }));
      return <Select data={opts} clearable searchable {...props} />;
    }
    default:
      return <TextInput {...props} />;
  }
}

export function CrudResource({ title, path, idKey = "id", columns, fields, editable, deletable }: Props) {
  const list = useList<any>(path);
  const [editing, setEditing] = useState<any | null>(null); // row being edited, or {} for create
  const isCreate = editing && !editing[idKey];

  const form = useForm<Record<string, any>>({ initialValues: {} });

  const open = (row: any | null) => {
    const init: Record<string, any> = {};
    fields.forEach((f) => {
      const v = row ? row[f.name] : f.type === "checkbox" ? false : "";
      init[f.name] = v ?? (f.type === "checkbox" ? false : "");
    });
    form.setValues(init);
    setEditing(row ?? {});
  };

  const save = useApiMutation(
    (vals: Record<string, any>) => {
      const body = cleanup(vals, fields, !!isCreate);
      return isCreate ? api.post(path, body) : api.patch(`${path}/${editing[idKey]}`, body);
    },
    { invalidate: [path], success: isCreate ? "Created" : "Updated", onDone: () => setEditing(null) },
  );

  const remove = useApiMutation((row: any) => api.del(`${path}/${row[idKey]}`), {
    invalidate: [path],
    success: "Deleted",
  });

  return (
    <Stack>
      <Group justify="space-between">
        <Title order={3}>{title}</Title>
        <Button onClick={() => open(null)}>New</Button>
      </Group>

      {list.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              {columns.map((c) => (
                <Table.Th key={c.key}>{c.label}</Table.Th>
              ))}
              {(editable || deletable) && <Table.Th />}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {(list.data ?? []).map((row) => (
              <Table.Tr key={row[idKey]}>
                {columns.map((c) => (
                  <Table.Td key={c.key}>{c.render ? c.render(row) : formatCell(row[c.key])}</Table.Td>
                ))}
                {(editable || deletable) && (
                  <Table.Td>
                    <Group gap="xs" justify="flex-end">
                      {editable && (
                        <Button size="xs" variant="light" onClick={() => open(row)}>
                          Edit
                        </Button>
                      )}
                      {deletable && (
                        <ActionIcon
                          color="red"
                          variant="light"
                          onClick={() => {
                            if (confirm("Delete this row?")) remove.mutate(row);
                          }}
                        >
                          ✕
                        </ActionIcon>
                      )}
                    </Group>
                  </Table.Td>
                )}
              </Table.Tr>
            ))}
            {(list.data ?? []).length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={columns.length + 1}>
                  <Text c="dimmed" ta="center" py="md">No rows yet.</Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}

      <Modal
        opened={!!editing}
        onClose={() => setEditing(null)}
        title={isCreate ? `New ${title}` : `Edit ${title}`}
        centered
      >
        <form onSubmit={form.onSubmit((vals) => save.mutate(vals))}>
          <Stack>
            {fields
              .filter((f) => (isCreate ? f.inCreate !== false : f.inEdit !== false))
              .map((f) => (
                <FieldInput key={f.name} field={f} form={form} />
              ))}
            <Group justify="flex-end">
              <Button variant="default" onClick={() => setEditing(null)}>Cancel</Button>
              <Button type="submit" loading={save.isPending}>Save</Button>
            </Group>
          </Stack>
        </form>
      </Modal>
    </Stack>
  );
}

function formatCell(v: unknown): ReactNode {
  if (v === null || v === undefined || v === "") return <Text c="dimmed">—</Text>;
  if (typeof v === "boolean") return <Badge color={v ? "green" : "gray"}>{v ? "yes" : "no"}</Badge>;
  return String(v);
}

// Drop empty strings (so optional fields aren't sent as ""), coerce nothing else.
function cleanup(vals: Record<string, any>, fields: Field[], isCreate: boolean): Record<string, any> {
  const out: Record<string, any> = {};
  for (const f of fields) {
    if (isCreate ? f.inCreate === false : f.inEdit === false) continue;
    const v = vals[f.name];
    if (v === "" || v === undefined) {
      if (isCreate && f.required) out[f.name] = v;
      continue;
    }
    out[f.name] = v;
  }
  return out;
}
