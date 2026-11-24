import type { ReactNode } from "react";
import { Logo } from "../components/Logo";

/** The card every sign-in screen sits in. */
export function Frame({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="grid min-h-screen place-items-center bg-background p-4">
      <div className="w-[400px] max-w-full overflow-hidden rounded-lg border bg-card">
        {/* depth-ladder accent: green "bids" → amber "asks" */}
        <div className="h-0.5" style={{ background: "linear-gradient(90deg, var(--buy) 0 55%, var(--brand-amber) 55% 100%)" }} />
        <div className="flex flex-col gap-5 p-7">
          <Logo size={17} />
          <h1 className="text-lg font-semibold tracking-tight">{title}</h1>
          {children}
        </div>
      </div>
    </div>
  );
}
