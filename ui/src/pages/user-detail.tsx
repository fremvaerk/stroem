import { useCallback, useEffect, useRef, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { InfoGrid } from "@/components/info-grid";
import { LoadingSpinner } from "@/components/loading-spinner";
import { useTitle } from "@/hooks/use-title";
import { useAuth } from "@/context/auth-context";
import {
  getUser,
  setUserAdmin,
  setUserGroups,
  listGroups,
} from "@/lib/api";
import { formatTime } from "@/lib/formatting";
import type { UserDetail } from "@/lib/types";

export function UserDetailPage() {
  const { id } = useParams<{ id: string }>();
  const { user: currentUser, isAdmin } = useAuth();
  const [user, setUser] = useState<UserDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  // Admin card state
  const [adminSaving, setAdminSaving] = useState(false);
  const [adminError, setAdminError] = useState("");

  // Groups state
  const [groups, setGroups] = useState<string[]>([]);
  const [groupInput, setGroupInput] = useState("");
  const [groupSuggestions, setGroupSuggestions] = useState<string[]>([]);
  const [allGroups, setAllGroups] = useState<string[]>([]);
  const [groupSaving, setGroupSaving] = useState(false);
  const [groupError, setGroupError] = useState("");
  const [showSuggestions, setShowSuggestions] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useTitle(user ? `User: ${user.name || user.email}` : "User");

  const load = useCallback(async () => {
    if (!id) return;
    try {
      const data = await getUser(id);
      setUser(data);
      setGroups(data.groups ?? []);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load user");
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    if (!isAdmin) return;
    listGroups()
      .then((data) => setAllGroups(data.groups))
      .catch(() => {});
  }, [isAdmin]);

  async function handleAdminToggle(checked: boolean) {
    if (!user || !id) return;
    setAdminSaving(true);
    setAdminError("");
    try {
      await setUserAdmin(id, checked);
      setUser((prev) => (prev ? { ...prev, is_admin: checked } : prev));
    } catch (err) {
      setAdminError(
        err instanceof Error ? err.message : "Failed to update admin status",
      );
    } finally {
      setAdminSaving(false);
    }
  }

  async function handleRemoveGroup(group: string) {
    if (!id || groupSaving) return;
    setGroupSaving(true);
    setGroupError("");
    const next = groups.filter((g) => g !== group);
    try {
      await setUserGroups(id, next);
      setGroups(next);
    } catch (err) {
      setGroupError(
        err instanceof Error ? err.message : "Failed to update groups",
      );
    } finally {
      setGroupSaving(false);
    }
  }

  async function handleAddGroup() {
    const trimmed = groupInput.trim();
    if (!trimmed || !id || groupSaving || groups.includes(trimmed)) {
      setGroupInput("");
      setShowSuggestions(false);
      return;
    }
    setGroupSaving(true);
    setGroupError("");
    const next = [...groups, trimmed];
    try {
      await setUserGroups(id, next);
      setGroups(next);
      setGroupInput("");
      setShowSuggestions(false);
    } catch (err) {
      setGroupError(
        err instanceof Error ? err.message : "Failed to update groups",
      );
    } finally {
      setGroupSaving(false);
    }
  }

  function handleGroupInputChange(value: string) {
    setGroupInput(value);
    if (value.trim()) {
      const lower = value.toLowerCase();
      const suggestions = allGroups.filter(
        (g) => g.toLowerCase().includes(lower) && !groups.includes(g),
      );
      setGroupSuggestions(suggestions);
      setShowSuggestions(suggestions.length > 0);
    } else {
      setGroupSuggestions([]);
      setShowSuggestions(false);
    }
  }

  function handleSuggestionClick(suggestion: string) {
    setGroupInput(suggestion);
    setShowSuggestions(false);
    inputRef.current?.focus();
  }

  function handleGroupKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter") {
      e.preventDefault();
      handleAddGroup();
    } else if (e.key === "Escape") {
      setShowSuggestions(false);
    }
  }

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

  const isSelf = currentUser?.user_id === user.user_id;
  const showAdminCard = isAdmin;

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
            label: "Role",
            value: user.is_admin ? (
              <Badge className="text-xs">Admin</Badge>
            ) : (
              <span className="text-muted-foreground">User</span>
            ),
          },
          {
            label: "Groups",
            value:
              (user.groups?.length ?? 0) > 0 ? (
                <div className="flex flex-wrap gap-1">
                  {(user.groups ?? []).map((g) => (
                    <Badge key={g} variant="outline" className="text-xs">
                      {g}
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

      {showAdminCard && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Administration</CardTitle>
          </CardHeader>
          <CardContent className="space-y-6">
            {adminError && (
              <p className="text-sm text-destructive">{adminError}</p>
            )}

            {/* Admin toggle */}
            <div className="flex items-start justify-between gap-4">
              <div>
                <Label htmlFor="admin-switch" className="text-sm font-medium">
                  Administrator
                </Label>
                <p className="mt-0.5 text-xs text-muted-foreground">
                  Admins bypass all ACL rules and can manage users.
                </p>
              </div>
              <Switch
                id="admin-switch"
                checked={user.is_admin ?? false}
                onCheckedChange={handleAdminToggle}
                disabled={adminSaving || isSelf}
              />
            </div>

            {/* Group management */}
            <div className="space-y-3">
              <Label className="text-sm font-medium">Groups</Label>

              {groupError && (
                <p className="text-sm text-destructive">{groupError}</p>
              )}

              {groups.length > 0 && (
                <div className="flex flex-wrap gap-1.5">
                  {groups.map((g) => (
                    <span
                      key={g}
                      className="inline-flex items-center gap-1 rounded-md border px-2 py-0.5 text-xs font-medium"
                    >
                      {g}
                      <button
                        type="button"
                        onClick={() => handleRemoveGroup(g)}
                        disabled={groupSaving}
                        className="ml-0.5 rounded hover:text-destructive disabled:opacity-50"
                        aria-label={`Remove group ${g}`}
                      >
                        <X className="h-3 w-3" />
                      </button>
                    </span>
                  ))}
                </div>
              )}

              <div className="relative flex gap-2">
                <div className="relative flex-1">
                  <Input
                    ref={inputRef}
                    placeholder="Add group..."
                    value={groupInput}
                    onChange={(e) => handleGroupInputChange(e.target.value)}
                    onKeyDown={handleGroupKeyDown}
                    onBlur={() =>
                      setTimeout(() => setShowSuggestions(false), 150)
                    }
                    disabled={groupSaving}
                    className="h-8 text-sm"
                  />
                  {showSuggestions && (
                    <div className="absolute z-10 mt-1 w-full rounded-md border bg-popover shadow-md">
                      {groupSuggestions.map((s) => (
                        <button
                          key={s}
                          type="button"
                          onMouseDown={() => handleSuggestionClick(s)}
                          className="w-full px-3 py-1.5 text-left text-sm hover:bg-accent"
                        >
                          {s}
                        </button>
                      ))}
                    </div>
                  )}
                </div>
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={handleAddGroup}
                  disabled={groupSaving || !groupInput.trim()}
                  className="h-8"
                >
                  Add
                </Button>
              </div>
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
