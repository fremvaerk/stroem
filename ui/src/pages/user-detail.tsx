import { useCallback, useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { InfoGrid } from "@/components/info-grid";
import { LoadingSpinner } from "@/components/loading-spinner";
import { useTitle } from "@/hooks/use-title";
import { getUser } from "@/lib/api";
import { formatTime } from "@/lib/formatting";
import type { UserDetail } from "@/lib/types";

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
    return <LoadingSpinner />;
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

      <InfoGrid
        columns={4}
        items={[
          {
            label: "User ID",
            value: (
              <span className="truncate font-mono">
                {user.user_id.substring(0, 8)}...
              </span>
            ),
          },
          {
            label: "Email",
            value: <span className="truncate">{user.email}</span>,
          },
          {
            label: "Auth Methods",
            value:
              user.auth_methods.length > 0 ? (
                <div className="flex flex-wrap gap-1">
                  {user.auth_methods.map((method) => (
                    <Badge key={method} variant="outline" className="text-xs">
                      {method}
                    </Badge>
                  ))}
                </div>
              ) : (
                <span className="text-muted-foreground">-</span>
              ),
          },
          {
            label: "Last Login",
            value: user.last_login_at ? formatTime(user.last_login_at) : "-",
          },
          { label: "Created", value: formatTime(user.created_at) },
        ]}
      />
    </div>
  );
}
