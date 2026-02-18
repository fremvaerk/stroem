import { useCallback, useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useTitle } from "@/hooks/use-title";
import { getUser } from "@/lib/api";
import type { UserDetail } from "@/lib/types";

function formatTime(dateStr: string): string {
  return new Date(dateStr).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function UserDetailPage() {
  const { id } = useParams<{ id: string }>();
  const [user, setUser] = useState<UserDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  useTitle(user ? `User: ${user.name || user.email}` : "User");

  const load = useCallback(async () => {
    if (!id) return;
    try {
      const data = await getUser(id);
      setUser(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load user");
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    load();
  }, [load]);

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  if (error || !user) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">
          {error || "User not found"}
        </p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/users">Back to users</Link>
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to="/users">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex-1">
          <h1 className="text-2xl font-semibold tracking-tight">
            {user.name || user.email}
          </h1>
          <p className="mt-0.5 font-mono text-xs text-muted-foreground">
            {user.user_id}
          </p>
        </div>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <div className="rounded-lg border px-4 py-3">
          <p className="text-xs text-muted-foreground">User ID</p>
          <p className="mt-0.5 truncate font-mono text-sm font-medium">
            {user.user_id.substring(0, 8)}...
          </p>
        </div>
        <div className="rounded-lg border px-4 py-3">
          <p className="text-xs text-muted-foreground">Email</p>
          <p className="mt-0.5 truncate text-sm font-medium">{user.email}</p>
        </div>
        <div className="rounded-lg border px-4 py-3">
          <p className="text-xs text-muted-foreground">Auth Methods</p>
          <div className="mt-0.5 flex flex-wrap gap-1">
            {user.auth_methods.length > 0 ? (
              user.auth_methods.map((method) => (
                <Badge key={method} variant="outline" className="text-xs">
                  {method}
                </Badge>
              ))
            ) : (
              <span className="text-sm text-muted-foreground">-</span>
            )}
          </div>
        </div>
        <div className="rounded-lg border px-4 py-3">
          <p className="text-xs text-muted-foreground">Created</p>
          <p className="mt-0.5 text-sm font-medium">
            {formatTime(user.created_at)}
          </p>
        </div>
      </div>
    </div>
  );
}
