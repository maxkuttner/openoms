import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../../api/client";
import { authApi } from "../../login/api";
import type { AuthMethods } from "../../login";
import { Notice, Spinner } from "../../components/common";
import { Pill } from "../../components/status";

type AuthStatus = {
  configured: boolean;
  issuer: string | null;
  discovery_ok: boolean;
  error: string | null;
  redirect_uri: string | null;
};

/** A two-column key/value table. */
function Facts({ rows }: { rows: [ReactNode, ReactNode][] }) {
  return (
    <dl className="grid grid-cols-[200px_1fr] rounded-md border">
      {rows.map(([k, v], i) => (
        <div key={i} className="contents">
          <dt className="border-b border-border-subtle px-3 py-[5px] text-muted-foreground last-of-type:border-0">{k}</dt>
          <dd className="border-b border-border-subtle px-3 py-[5px]">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function SignInPage() {
  const status = useQuery<AuthStatus>({ queryKey: ["/admin/auth/status"], queryFn: () => api.get<AuthStatus>("/admin/auth/status") });
  const s = status.data;
  const on = !!s && s.configured && s.discovery_ok;
  const methods = useQuery<AuthMethods>({ queryKey: ["/auth/methods"], queryFn: authApi.methods });
  const m = methods.data;
  const onOff = (v: boolean) => <Pill tone={v ? "buy" : "muted"}>{v ? "on" : "off"}</Pill>;
  const dash = <span className="text-muted-foreground">—</span>;

  return (
    <div className="flex max-w-3xl flex-col gap-3">
      <h2 className="text-sm font-semibold">Sign-in</h2>
      {m && <Facts rows={[["Email and password", onOff(m.local)], ["Single sign-on (SSO)", onOff(m.sso)]]} />}
      <div className="flex items-center gap-2">
        <h3 className="font-semibold">SSO</h3>
        {s && onOff(on)}
      </div>
      {status.isLoading && <Spinner />}
      {s && !s.configured && (
        <Notice tone="info" title="SSO is not configured">
          Optional. To offer "Sign in with SSO" with any OpenID Connect provider, add an <code className="num">[auth.oidc]</code> section
          to the OMS config, set <code className="num">OMS_OIDC_CLIENT_SECRET</code>, and restart. Register the callback address below
          with your identity provider.
        </Notice>
      )}
      {s && s.configured && !s.discovery_ok && (
        <Notice title="SSO is configured but not working">
          {s.error ?? "The SSO provider could not be reached at startup."} Fix it and restart the OMS.
        </Notice>
      )}
      {s && (
        <Facts rows={[
          ["Identity provider", s.issuer ?? dash],
          ["Callback address", s.redirect_uri ? <code className="num">{s.redirect_uri}</code> : dash],
        ]} />
      )}
    </div>
  );
}
