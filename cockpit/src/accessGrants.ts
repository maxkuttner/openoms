// Pure helpers for access grants: what a group (or everyone) may use.

export type GrantState = { key: string; on: boolean; inherited: boolean };

/** Default grants first (on, inherited: a group cannot remove them), then the group's own. */
export function resolveGrants(defaults: string[], group: string[]): GrantState[] {
  const inherited = new Set(defaults);
  const keys = [...new Set([...defaults, ...group])];
  return keys.map((key) => ({ key, on: true, inherited: inherited.has(key) }));
}

/** Set one key on or off in a grant list, never duplicating. */
export function toggleGrant(grants: string[], key: string, on: boolean): string[] {
  const rest = grants.filter((g) => g !== key);
  return on ? [...rest, key] : rest;
}
