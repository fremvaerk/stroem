import { useEffect, useMemo } from "react";
import { Link } from "react-router";
import { Zap } from "lucide-react";
import { setTokensFromOidc } from "@/lib/api";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

function parseCallbackHash() {
  const hash = window.location.hash.substring(1);
  const params = new URLSearchParams(hash);
  return {
    error: params.get("error"),
    accessToken: params.get("access_token"),
    refreshToken: params.get("refresh_token"),
  };
}

export function LoginCallbackPage() {
  const { error: urlError, accessToken, refreshToken } = useMemo(() => parseCallbackHash(), []);

  const error = urlError ?? (!accessToken || !refreshToken ? "Missing tokens in callback" : null);

  useEffect(() => {
    if (accessToken && refreshToken) {
      setTokensFromOidc(accessToken, refreshToken);
      window.location.href = "/";
    }
  }, [accessToken, refreshToken]);

  if (!error) {
    return (
      <div className="flex h-screen items-center justify-center">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
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
