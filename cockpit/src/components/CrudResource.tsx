import { useRef, useState, type ReactNode } from "react";
import {
  Group, Title, Button, Table, Modal, Stack, TextInput, NumberInput,
  Select, Checkbox, ActionIcon, Text, Loader, Badge,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { api } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import { InstrumentSelect } from "./InstrumentSelect";
import { useLifecycle } from "./Lifecycle";
import type { Resource } from "../lifecycle";
import { initialFormValues, requestBody } from "../crud";

export type Field = {
  name: string;
  label: string;
  type?: "text" | "number" | "select" | "checkbox" | "instrument";
  // Fixed options, or options that depend on the other fields' values.
  options?: Option[] | ((values: Record<string, any>) => Option[]);
  // A line under the input; may depend on the other fields' values.
  description?: (values: Record<string, any>) => ReactNode;
  // Populate a select from another list endpoint, mapping each row to {value,label}.
  optionsPath?: string;
  optionValue?: string;
  optionLabel?: string;
  required?: boolean;
  default?: string; // starting value for a new row
  nullable?: boolean; // emptied on edit → sent as null, so the server clears it
  inCreate?: boolean; // default true
  inEdit?: boolean; // default true
};

type Option = { value: string; label: string };

export type Column = { key: string; label: string; render?: (row: any) => ReactNode };

type Props = {
  title: string;
  path: string; // collection path, e.g. "/admin/accounts"
  idKey?: string; // default "id"
  columns: Column[];
  fields: Field[];
  editable?: boolean;
  deletable?: boolean;
  // Extra per-row action rendered in the action cell, before Edit. Additive —
  // omit it and nothing changes.
  rowAction?: (row: any) => ReactNode;
  // Rows are deactivated instead of deleted: checkboxes, a Deactivate bar,
  // inactive rows hidden behind a "Show inactive" switch, and Reactivate.
  lifecycle?: { resource: Resource; afterChange?: () => Promise<unknown> };
  // Create form only: given the values before and after a change, the fields
  // to set as a consequence (e.g. a code derived from two selects).
  derive?: (previous: Record<string, any>, next: Record<string, any>) => Record<string, any>;
  // Called with the created row once a create succeeds.
  onCreated?: (row: any) => void;
};

// Render the value of one field, resolving select options (incl. async optionsPath).
function FieldInput({ field, form }: { field: Field; form: any }) {
  const remote = useList<any>(field.optionsPath ?? "", !!field.optionsPath);
  const props = { label: field.label, description: field.description?.(form.values), ...form.getInputProps(field.name) };
  switch (field.type) {
    case "instrument":
      return (
        <InstrumentSelect
          label={field.label}
          required={field.required}
          value={(form.values[field.name] as string) || null}
          onChange={(v) => form.setFieldValue(field.name, v ?? "")}
          apiGet={api.get}
        />
      );
    case "number":
      return <NumberInput {...props} />;
    case "checkbox":
      return (
        <Checkbox
          label={field.label}
          description={props.description}
          {...form.getInputProps(field.name, { type: "checkbox" })}
        />
      );
    case "select": {
      const opts =
        (typeof field.options === "function" ? field.options(form.values) : field.options) ??
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

export function CrudResource({
  title, path, idKey = "id", columns, fields, editable, deletable, rowAction, lifecycle, derive, onCreated,
}: Props) {
  const list = useList<any>(path);
  const life = useLifecycle(list.data, {
    resource: lifecycle?.resource ?? "principal",
    path,
    idKey,
    label: (r) => String(r.code ?? r[idKey]),
    afterChange: lifecycle?.afterChange,
  });
  const rows = lifecycle ? life.visible : (list.data ?? []);
  const [editing, setEditing] = useState<any | null>(null); // row being edited, or {} for create
  const isCreate = editing && !editing[idKey];

  const creating = useRef(false);
  creating.current = !!isCreate;
  const form = useForm<Record<string, any>>({
    initialValues: {},
    onValuesChange: (next, previous) => {
      if (!creating.current || !derive) return;
      for (const [k, v] of Object.entries(derive(previous, next))) {
        if (next[k] !== v) form.setFieldValue(k, v);
      }
    },
  });

  const open = (row: any | null) => {
    form.setValues(initialFormValues(fields, row));
    setEditing(row ?? {});
  };

  const save = useApiMutation(
    (vals: Record<string, any>) => {
      const body = requestBody(vals, fields, !!isCreate);
      return isCreate ? api.post(path, body) : api.patch(`${path}/${editing[idKey]}`, body);
    },
    {
      invalidate: [path],
      success: isCreate ? "Created" : "Updated",
      onDone: (row) => {
        setEditing(null);
        if (isCreate) onCreated?.(row);
      },
    },
  );

  const remove = useApiMutation((row: any) => api.del(`${path}/${row[idKey]}`), {
    invalidate: [path],
    success: "Deleted",
  });

  return (
    <Stack>
      <Group justify="space-between">
        <Title order={3}>{title}</Title>
        <Group>
          {lifecycle && life.switchControl}
          <Button onClick={() => open(null)}>New</Button>
        </Group>
      </Group>
      {lifecycle && life.selectionBar}

      {list.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              {lifecycle && <Table.Th w={40}>{life.headerCheckbox}</Table.Th>}
              {columns.map((c) => (
                <Table.Th key={c.key}>{c.label}</Table.Th>
              ))}
              {(editable || deletable || rowAction) && <Table.Th />}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {rows.map((row) => (
              <Table.Tr key={row[idKey]} c={lifecycle && !life.isActive(row) ? "dimmed" : undefined}>
                {lifecycle && <Table.Td>{life.rowCheckbox(row)}</Table.Td>}
                {columns.map((c) => (
                  <Table.Td key={c.key}>{c.render ? c.render(row) : formatCell(row[c.key])}</Table.Td>
                ))}
                {(editable || deletable || rowAction) && (
                  <Table.Td>
                    <Group gap="xs" justify="flex-end">
                      {rowAction?.(row)}
                      {lifecycle && !life.isActive(row)
                        ? life.reactivateButton(row)
                        : editable && (
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
            {rows.length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={columns.length + 2}>
                  <Text c="dimmed" ta="center" py="md">No rows yet.</Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}

      {lifecycle && life.confirmModal}

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

