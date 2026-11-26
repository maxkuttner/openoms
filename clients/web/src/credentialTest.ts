// What a credential test or save says, as the cockpit shows it. Kept out of
// CredentialsPanel.tsx so `npm test` (node --test) can import it.
import type { SaveResponse, TestResponse } from "./api/types";

export type TestTone = "info" | "success" | "warning" | "error";

/** A test result as a notice: a warning (venue not ready, saving still
 * allowed) is neither a pass nor a failure; a pass shows what it found. */
export function describeTest(r: Pick<TestResponse, "tested" | "ok" | "message" | "warning">): { tone: TestTone; text: string } {
  if (!r.tested) return { tone: "info", text: r.message ?? "not testable before save" };
  if (r.warning) return { tone: "warning", text: r.message ?? "the venue is not ready yet" };
  if (r.ok) return { tone: "success", text: r.message ?? "credential verified" };
  return { tone: "error", text: r.message ?? "credential test failed" };
}

/** What the test before a save found, as one sentence for the save toast and notice. */
export function describeSaveTest(r: Pick<SaveResponse, "tested" | "message" | "warning">): { tone: TestTone; text: string } {
  if (!r.tested) return { tone: "info", text: `Not tested before save (${r.message}).` };
  if (r.warning) return { tone: "warning", text: r.message ?? "The venue is not ready yet." };
  return { tone: "info", text: r.message ?? "Credential verified." };
}
