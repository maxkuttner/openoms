import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table";
import { ConfirmButton } from "./ConfirmButton";
import { CopyText, FormRow, Notice, Section, Spinner } from "./common";

/** One API key as the box shows it; `secret` only right after creation. */
export type ApiKey = { key_id: string; name: string | null; created_at: string; secret?: string | null };

/** Where the box reads and writes keys: your own (Trade → API) or one user's (Admin Panel). */
export type ApiKeysSource = {
  list: () => Promise<ApiKey[]>;
  create: (name: string) => Promise<ApiKey>;
  revoke: (keyId: string) => Promise<unknown>;
  errorText: (e: unknown) => string;
};

/**
 * "API keys": list, create and revoke the keys that act as one user, with that
 * user's group access. Used by Trade → API (your own keys) and the Admin Panel's
 * API page (any user's keys).
 */
export function ApiKeysBox({ source, intro, idPrefix = "key", canCreate = true }: { source: ApiKeysSource; intro: ReactNode; idPrefix?: string; canCreate?: boolean }) {
  const [keys, setKeys] = useState<ApiKey[] | null>(null);
  const [name, setName] = useState("");
  const [created, setCreated] = useState<ApiKey | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Loads once per mount: callers give the box a `key` per user, so switching
  // users mounts a fresh box rather than re-running this on every render.
  const src = useRef(source);
  src.current = source;
  const load = () => src.current.list().then(setKeys, (e) => setError(src.current.errorText(e)));
  useEffect(() => { void load(); }, []);

  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try { await fn(); await load(); } catch (e) { setError(src.current.errorText(e)); }
  };

  return (
    <Section title="API keys">
      <p className="text-muted-foreground">{intro}</p>
      {error && <Notice>{error}</Notice>}
      {created?.secret && (
        <Notice tone="success" title="Copy the secret now: it is shown once">
          <CopyText value={`${created.key_id}.${created.secret}`} />
        </Notice>
      )}
      {canCreate && <form className="flex items-end gap-2"
        onSubmit={(e) => { e.preventDefault(); void act(async () => { setCreated(await src.current.create(name.trim())); setName(""); }); }}>
        <FormRow label="Name" htmlFor={`${idPrefix}-name`}>
          <Input id={`${idPrefix}-name`} value={name} onChange={(e) => setName(e.currentTarget.value)} />
        </FormRow>
        <Button type="submit">Create key</Button>
      </form>}
      {keys === null ? <Spinner /> : keys.length === 0 ? <p className="text-muted-foreground">No keys.</p> : (
        <Table>
          <TableBody>
            {keys.map((k) => (
              <TableRow key={k.key_id}>
                <TableCell><code className="num">{k.key_id}</code></TableCell>
                <TableCell>{k.name ?? "—"}</TableCell>
                <TableCell className="num">{new Date(k.created_at).toLocaleDateString()}</TableCell>
                <TableCell className="text-right">
                  <ConfirmButton size="xs" variant="destructive" title={`Revoke ${k.key_id}?`} confirmLabel="Revoke"
                    onConfirm={() => act(() => src.current.revoke(k.key_id))}>Revoke</ConfirmButton>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
    </Section>
  );
}
