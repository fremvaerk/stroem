import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import { X, UserPlus } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Checkbox } from "@/components/ui/checkbox";
import { PaginationControls } from "@/components/pagination-controls";
import { LoadingSpinner } from "@/components/loading-spinner";
import { useTitle } from "@/hooks/use-title";
import { useAuth } from "@/context/auth-context";
import { createUser, listGroups, listUsers, ApiError } from "@/lib/api";
import type { UserListItem } from "@/lib/types";

const PAGE_SIZE = 20;

function formatTime(dateStr: string): string {
  return new Date(dateStr).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function UsersPage() {
  useTitle("Users");
  const { isAdmin } = useAuth();
  const [users, setUsers] = useState<UserListItem[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [offset, setOffset] = useState(0);
  const [forbidden, setForbidden] = useState(false);

  const load = useCallback(async () => {
    try {
      const data = await listUsers(PAGE_SIZE, offset);
      setUsers(data.items);
      setTotal(data.total);
    } catch {
      setForbidden(true);
    } finally {
      setLoading(false);
    }
  }, [offset]);

  useEffect(() => {
    setLoading(true);
    load();
  }, [load]);

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Users</h1>
          <p className="text-sm text-muted-foreground">
            Registered user accounts
          </p>
        </div>
        {isAdmin && !forbidden && <AddUserButton onCreated={load} />}
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">All Users</CardTitle>
        </CardHeader>
        <CardContent>
          {forbidden ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              Admin access required to view users.
            </p>
          ) : loading ? (
            <LoadingSpinner />
          ) : users.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No users found.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Email</TableHead>
                  <TableHead>Auth Method</TableHead>
                  {isAdmin && <TableHead>Role</TableHead>}
                  {isAdmin && <TableHead>Groups</TableHead>}
                  <TableHead>Last Login</TableHead>
                  <TableHead>Created</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {users.map((u) => (
                  <TableRow key={u.user_id}>
                    <TableCell>
                      <Link
                        to={`/users/${u.user_id}`}
                        className="font-medium hover:underline"
                      >
                        {u.name || u.email}
                      </Link>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {u.email}
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {u.auth_methods.map((method) => (
                          <Badge
                            key={method}
                            variant="outline"
                            className="text-xs"
                          >
                            {method}
                          </Badge>
                        ))}
                      </div>
                    </TableCell>
                    {isAdmin && (
                      <TableCell>
                        {u.is_admin ? (
                          <Badge className="text-xs">Admin</Badge>
                        ) : (
                          <span className="text-sm text-muted-foreground">
                            User
                          </span>
                        )}
                      </TableCell>
                    )}
                    {isAdmin && (
                      <TableCell>
                        {u.groups && u.groups.length > 0 ? (
                          <div className="flex flex-wrap gap-1">
                            {u.groups.map((g) => (
                              <Badge
                                key={g}
                                variant="outline"
                                className="text-xs"
                              >
                                {g}
                              </Badge>
                            ))}
                          </div>
                        ) : (
                          <span className="text-muted-foreground">-</span>
                        )}
                      </TableCell>
                    )}
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {u.last_login_at ? formatTime(u.last_login_at) : "-"}
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatTime(u.created_at)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}

          <PaginationControls
            offset={offset}
            pageSize={PAGE_SIZE}
            total={total}
            onPrevious={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
            onNext={() => setOffset((o) => o + PAGE_SIZE)}
          />
        </CardContent>
      </Card>
    </div>
  );
}

/**
 * Add-user dialog. Creates a user record ahead of first OIDC login so
 * group memberships are already in place when they authenticate. Mirrors
 * the group-autocomplete UX from user-detail so admins get suggestions
 * for existing group names as they type.
 */
function AddUserButton({ onCreated }: { onCreated: () => void }) {
  const [open, setOpen] = useState(false);
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [isAdmin, setIsAdmin] = useState(false);
  const [groups, setGroups] = useState<string[]>([]);
  const [groupInput, setGroupInput] = useState("");
  const [allGroups, setAllGroups] = useState<string[]>([]);
  const [showSuggestions, setShowSuggestions] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Load known group names once when the dialog opens so autocomplete
  // works without pre-loading on the users page.
  useEffect(() => {
    if (!open) return;
    listGroups()
      .then((data) => setAllGroups(data.groups))
      .catch(() => setAllGroups([]));
  }, [open]);

  function reset() {
    setEmail("");
    setName("");
    setIsAdmin(false);
    setGroups([]);
    setGroupInput("");
    setShowSuggestions(false);
    setError(null);
  }

  const suggestions = useMemo(() => {
    const query = groupInput.trim().toLowerCase();
    if (!query) return [];
    return allGroups
      .filter(
        (g) => !groups.includes(g) && g.toLowerCase().includes(query),
      )
      .slice(0, 5);
  }, [groupInput, allGroups, groups]);

  function commitGroup(raw: string) {
    const g = raw.trim();
    if (!g) return;
    if (groups.includes(g)) {
      setGroupInput("");
      return;
    }
    setGroups((prev) => [...prev, g]);
    setGroupInput("");
    setShowSuggestions(false);
  }

  function handleGroupKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      commitGroup(groupInput);
    } else if (
      e.key === "Backspace" &&
      groupInput === "" &&
      groups.length > 0
    ) {
      setGroups((prev) => prev.slice(0, -1));
    }
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    setError(null);

    // If the admin left an unfinished tag in the input, treat Enter-on-submit
    // as commit-then-submit so we don't silently drop the group.
    const pending = groupInput.trim();
    const finalGroups = pending && !groups.includes(pending) ? [...groups, pending] : groups;

    setSubmitting(true);
    try {
      await createUser({
        email: email.trim(),
        name: name.trim() || undefined,
        groups: finalGroups,
        is_admin: isAdmin,
      });
      setOpen(false);
      reset();
      onCreated();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "Failed to create user";
      setError(msg);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) reset();
      }}
    >
      <DialogTrigger asChild>
        <Button size="sm">
          <UserPlus className="mr-1.5 h-4 w-4" />
          Add user
        </Button>
      </DialogTrigger>
      <DialogContent>
        <form onSubmit={handleSubmit}>
          <DialogHeader>
            <DialogTitle>Add user</DialogTitle>
            <DialogDescription>
              Pre-provision a user so their groups are assigned before they
              first sign in. They&rsquo;ll authenticate via OIDC.
            </DialogDescription>
          </DialogHeader>

          <div className="my-4 space-y-4">
            <div className="space-y-2">
              <Label htmlFor="add-user-email">Email</Label>
              <Input
                id="add-user-email"
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="alice@example.com"
                required
                autoFocus
              />
            </div>

            <div className="space-y-2">
              <Label htmlFor="add-user-name">Display name (optional)</Label>
              <Input
                id="add-user-name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Alice Smith"
              />
            </div>

            <div className="space-y-2">
              <Label>Groups</Label>
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
                        onClick={() =>
                          setGroups((prev) => prev.filter((x) => x !== g))
                        }
                        className="ml-0.5 rounded hover:text-destructive"
                        aria-label={`Remove group ${g}`}
                      >
                        <X className="h-3 w-3" />
                      </button>
                    </span>
                  ))}
                </div>
              )}
              <div className="relative">
                <Input
                  ref={inputRef}
                  placeholder="Add group (Enter to confirm)"
                  value={groupInput}
                  onChange={(e) => {
                    setGroupInput(e.target.value);
                    setShowSuggestions(true);
                  }}
                  onKeyDown={handleGroupKeyDown}
                  onFocus={() => setShowSuggestions(true)}
                  onBlur={() =>
                    setTimeout(() => setShowSuggestions(false), 150)
                  }
                  className="h-8 text-sm"
                />
                {showSuggestions && suggestions.length > 0 && (
                  <div className="absolute z-10 mt-1 w-full rounded-md border bg-popover shadow-md">
                    {suggestions.map((s) => (
                      <button
                        key={s}
                        type="button"
                        onMouseDown={() => commitGroup(s)}
                        className="w-full px-3 py-1.5 text-left text-sm hover:bg-accent"
                      >
                        {s}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            </div>

            <div className="flex items-center gap-2">
              <Checkbox
                id="add-user-admin"
                checked={isAdmin}
                onCheckedChange={(v) => setIsAdmin(v === true)}
              />
              <Label
                htmlFor="add-user-admin"
                className="text-sm font-normal cursor-pointer"
              >
                Grant admin privileges
              </Label>
            </div>

            {error && (
              <p className="text-sm text-destructive" role="alert">
                {error}
              </p>
            )}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                setOpen(false);
                reset();
              }}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={submitting || !email.trim()}>
              {submitting ? "Creating…" : "Create user"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
