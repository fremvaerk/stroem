import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate } from "react-router";
import { Zap } from "lucide-react";
import { useAuth } from "@/context/auth-context";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { LoadingSpinner } from "@/components/loading-spinner";

function parseCallbackHash() {
  const hash = window.location.hash.substring(1);
  const params = new URLSearchParams(hash);
  return {
    error: params.get("error"),
    accessToken: params.get("access_token"),
  };
}

export function LoginCallbackPage() {
  const navigate = useNavigate();
  const { restoreFromOidc } = useAuth();
  const { error: urlError, accessToken } = useMemo(() => parseCallbackHash(), []);
  const [restoreError, setRestoreError] = useState<string | null>(null);

  // The refresh token is delivered as an HttpOnly cookie by the server redirect
  // — it never appears in the URL hash.
  const error = urlError ?? restoreError ?? (!accessToken ? "Missing access token in callback" : null);

  useEffect(() => {
    if (accessToken) {
      restoreFromOidc(accessToken)
        .then(() => navigate("/", { replace: true }))
        .catch(() => setRestoreError("Failed to restore session from OIDC callback"));
    }
  }, [accessToken, restoreFromOidc, navigate]);

  if (!error) {
    return <LoadingSpinner className="flex h-screen items-center justify-center" />;
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-muted/40 p-4">
      <Card className="w-full max-w-sm">
        <CardHeader className="items-center space-y-4 pb-2">
          <div className="flex h-12 w-12 items-center justify-center rounded-xl bg-destructive text-destructive-foreground">
            <Zap className="h-6 w-6" />
          </div>
          <div className="text-center">
            <h1 className="text-xl font-semibold tracking-tight">
              Login Failed
            </h1>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
            {error}
          </div>
          <Button asChild variant="outline" className="w-full">
            <Link to="/login">Back to login</Link>
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}
