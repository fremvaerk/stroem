import { useEffect, useState } from "react";
import { Navigate, useSearchParams } from "react-router";
import { Zap, ShieldAlert } from "lucide-react";
import { useAuth } from "@/context/auth-context";
import { useTitle } from "@/hooks/use-title";
import { LoadingSpinner } from "@/components/loading-spinner";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { apiFetch, ApiError } from "@/lib/api";

/**
 * OAuth 2.1 consent screen.
 *
 * Reached after the AS endpoint `/oauth/authorize` 302-redirects here with
 * the validated request params in the query string. We:
 *   1. Require an authenticated user session (redirect to /login otherwise).
 *   2. Look up the client's display info to show "Cursor wants to access...".
 *   3. On Allow: POST /api/oauth/consent to mint an auth code and navigate
 *      the browser to the final redirect URL.
 *   4. On Deny: build the spec-mandated error redirect ourselves and
 *      navigate to it — the OAuth client decides what to do with the
 *      `error=access_denied` response.
 */

interface ClientDescribe {
  client_id: string;
  client_name: string;
  is_dynamic: boolean;
  // Server-side check against the registered allowlist. The SPA never
  // sees the full list — that would let any authenticated user enumerate
  // every client's redirect URIs for phishing reconnaissance.
  redirect_uri_registered: boolean;
}

interface ConsentSuccess {
  redirect_url: string;
}

export function OAuthConsentPage() {
  useTitle("Authorize access");
  const { authRequired, isAuthenticated, isLoading } = useAuth();
  const [params] = useSearchParams();

  const [client, setClient] = useState<ClientDescribe | null>(null);
  const [clientError, setClientError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const clientId = params.get("client_id") ?? "";
  const redirectUri = params.get("redirect_uri") ?? "";
  const codeChallenge = params.get("code_challenge") ?? "";
  const codeChallengeMethod = params.get("code_challenge_method") ?? "S256";
  const scope = params.get("scope") ?? "mcp";
  const resource = params.get("resource") ?? "";
  const stateParam = params.get("state");

  // Look up the client's display info ahead of showing consent UI.
  // We pass the SPA-supplied `redirect_uri` so the server can tell us
  // whether to enable Allow/Deny — without returning the full list (which
  // would leak the client's registered callbacks to any authenticated
  // user, enabling typosquatting + targeted phishing).
  useEffect(() => {
    if (!isAuthenticated || !clientId) return;
    let cancelled = false;
    (async () => {
      try {
        const url = redirectUri
          ? `/api/oauth/clients/${encodeURIComponent(clientId)}?redirect_uri=${encodeURIComponent(redirectUri)}`
          : `/api/oauth/clients/${encodeURIComponent(clientId)}`;
        const data = await apiFetch<ClientDescribe>(url);
        if (!cancelled) setClient(data);
      } catch (e) {
        if (cancelled) return;
        const msg = e instanceof ApiError ? e.message : "Failed to load client";
        setClientError(msg);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [clientId, redirectUri, isAuthenticated]);

  if (isLoading) {
    return (
      <LoadingSpinner className="flex h-screen items-center justify-center" />
    );
  }

  // Round-trip through /login carrying the original consent URL so the user
  // lands back here after authenticating.
  if (authRequired && !isAuthenticated) {
    const here = `/consent?${params.toString()}`;
    return (
      <Navigate
        to={`/login?next=${encodeURIComponent(here)}`}
        replace
      />
    );
  }

  // Up-front parameter sanity check — the SPA must echo what /oauth/authorize
  // accepted; if any of these are missing the user can't make a real choice.
  const missing = [
    !clientId && "client_id",
    !redirectUri && "redirect_uri",
    !codeChallenge && "code_challenge",
    !resource && "resource",
  ].filter(Boolean);
  if (missing.length > 0) {
    return (
      <CenteredCard>
        <h2 className="text-lg font-medium">Invalid authorization request</h2>
        <p className="text-sm text-muted-foreground">
          Missing required parameter(s): {missing.join(", ")}.
        </p>
      </CenteredCard>
    );
  }

  async function handleAllow() {
    setSubmitting(true);
    try {
      const body = {
        client_id: clientId,
        redirect_uri: redirectUri,
        code_challenge: codeChallenge,
        code_challenge_method: codeChallengeMethod,
        scope,
        resource,
        state: stateParam,
      };
      const result = await apiFetch<ConsentSuccess>("/api/oauth/consent", {
        method: "POST",
        body: JSON.stringify(body),
      });
      // Replace the current history entry so the browser back-button doesn't
      // re-trigger consent.
      window.location.replace(result.redirect_url);
    } catch (e) {
      const msg = e instanceof ApiError ? e.message : "Consent failed";
      setClientError(msg);
      setSubmitting(false);
    }
  }

  function handleDeny() {
    // Only redirect when the redirect_uri is one the client registered;
    // otherwise an attacker could phish a logged-in user with
    // `/consent?redirect_uri=https://evil.example/...&...&client_id=<any>`
    // and turn the Deny button into an open redirect under the trusted
    // Strøm origin. If the URI is unregistered, surface an inline error
    // instead — the user has nowhere safe to send them.
    if (!client || !client.redirect_uri_registered) {
      setClientError(
        "Cannot send the denial response: redirect_uri is not registered for this client.",
      );
      return;
    }
    // RFC 6749 §4.1.2.1: communicate user refusal via the redirect URI.
    const sep = redirectUri.includes("?") ? "&" : "?";
    let url = `${redirectUri}${sep}error=access_denied`;
    if (stateParam) {
      url += `&state=${encodeURIComponent(stateParam)}`;
    }
    window.location.replace(url);
  }

  if (clientError) {
    return (
      <CenteredCard>
        <div className="flex items-center gap-2">
          <ShieldAlert className="size-5 text-destructive" />
          <h2 className="text-lg font-medium">Could not load client</h2>
        </div>
        <p className="text-sm text-muted-foreground">{clientError}</p>
      </CenteredCard>
    );
  }

  if (!client) {
    return (
      <LoadingSpinner className="flex h-screen items-center justify-center" />
    );
  }

  // Warn when the requested redirect_uri isn't in the registered list. The
  // backend rejects the consent POST in this case too — surface it early so
  // the user understands what's wrong before clicking. The server tells us
  // yes/no without exposing the full registered list.
  const redirectRegistered = client.redirect_uri_registered;

  return (
    <CenteredCard>
      <div className="mb-2 flex items-center gap-2">
        <Zap className="size-5 text-primary" />
        <span className="font-medium">Strøm MCP authorization</span>
      </div>
      <h2 className="text-lg font-semibold">
        Allow <span className="font-mono">{client.client_name}</span> to access
        your Strøm workspaces?
      </h2>
      <ul className="my-3 list-disc space-y-1 pl-5 text-sm text-muted-foreground">
        <li>List and execute tasks you have access to</li>
        <li>Read job status, logs, and artifacts</li>
        <li>Cancel running jobs</li>
      </ul>
      <p className="text-xs text-muted-foreground">
        Access is limited by your existing ACL permissions. The client will
        receive a token that expires in 1&nbsp;hour.
      </p>
      {client.is_dynamic && (
        <p className="mt-2 rounded bg-amber-100 px-3 py-2 text-xs text-amber-900">
          This client registered itself via Dynamic Client Registration. Make
          sure you recognise it before approving.
        </p>
      )}
      {!redirectRegistered && (
        <p className="mt-2 rounded bg-destructive/10 px-3 py-2 text-xs text-destructive">
          Warning: redirect_uri does not match a registered URL for this
          client. Approval will fail server-side.
        </p>
      )}
      <div className="mt-5 flex gap-2">
        <Button
          onClick={handleAllow}
          disabled={submitting || !redirectRegistered}
          className="flex-1"
        >
          {submitting ? "Authorizing…" : "Allow"}
        </Button>
        <Button
          variant="outline"
          onClick={handleDeny}
          disabled={submitting}
          className="flex-1"
        >
          Deny
        </Button>
      </div>
    </CenteredCard>
  );
}

function CenteredCard({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex min-h-screen items-center justify-center bg-muted/30 px-4">
      <Card className="w-full max-w-md">
        <CardHeader />
        <CardContent>{children}</CardContent>
      </Card>
    </div>
  );
}
