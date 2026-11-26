import { useState, type ReactNode } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { cn } from "@/lib/utils";

/** Inline spinner. */
export function Spinner({ className }: { className?: string }) {
  return <Loader2 aria-label="Loading" className={cn("size-4 animate-spin text-muted-foreground", className)} />;
}

/** A value shown as code with a Copy button next to it. */
export function CopyText({ value, label }: { value: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex flex-col gap-1">
      {label && <span className="text-xs text-muted-foreground">{label}</span>}
      <div className="flex items-start gap-2">
        <code className="num flex-1 rounded-md border bg-card px-2 py-1 text-xs whitespace-pre-wrap [overflow-wrap:anywhere]">{value}</code>
        <Button
          type="button"
          size="xs"
          variant="secondary"
          onClick={() => void navigator.clipboard.writeText(value).then(() => setCopied(true))}
        >
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
    </div>
  );
}

/** Error / success / info callouts. */
export function Notice({ tone = "error", title, children }: { tone?: "error" | "success" | "warning" | "info"; title?: ReactNode; children?: ReactNode }) {
  return (
    <Alert
      variant={tone === "error" ? "destructive" : "default"}
      className={cn(tone === "success" && "border-buy/40 text-buy", tone === "warning" && "border-warn/40 text-warn", tone === "info" && "text-foreground-2")}
    >
      {title && <AlertTitle>{title}</AlertTitle>}
      {children && <AlertDescription>{children}</AlertDescription>}
    </Alert>
  );
}

/** A labelled input row: label above, error below. */
export function FormRow({ label, error, children, htmlFor }: { label: ReactNode; error?: ReactNode; children: ReactNode; htmlFor?: string }) {
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={htmlFor} className="text-xs text-muted-foreground">{label}</label>
      {children}
      {error && <p role="alert" className="text-xs text-sell">{error}</p>}
    </div>
  );
}

/** A bordered panel section with a title. */
export function Section({ title, children, className }: { title?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={cn("rounded-md border bg-card p-4", className)}>
      {title && <h2 className="mb-3 text-sm font-semibold">{title}</h2>}
      <div className="flex flex-col gap-3">{children}</div>
    </section>
  );
}

/** A text button that looks like a link. */
export function LinkButton({ onClick, children }: { onClick: () => void; children: ReactNode }) {
  return (
    <button type="button" onClick={onClick} className="self-start text-xs text-primary hover:underline">
      {children}
    </button>
  );
}
