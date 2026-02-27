import { useCallback, useEffect, useState } from "react";
import { Copy, Plus, Trash2 } from "lucide-react";
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { useTitle } from "@/hooks/use-title";
import { listApiKeys, createApiKey, deleteApiKey } from "@/lib/api";
import type { ApiKey, CreateApiKeyResponse } from "@/lib/types";

function formatTime(dateStr: string): string {
  return new Date(dateStr).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function isExpired(expiresAt: string | null): boolean {
  if (!expiresAt) return false;
  return new Date(expiresAt) < new Date();
}

export function SettingsPage() {
  useTitle("Settings");
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  // Create dialog state
  const [createOpen, setCreateOpen] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyExpiry, setNewKeyExpiry] = useState("");
  const [creating, setCreating] = useState(false);

  // Reveal dialog state (shown after creation)
  const [createdKey, setCreatedKey] = useState<CreateApiKeyResponse | null>(
    null,
  );
  const [copied, setCopied] = useState(false);

  // Delete confirmation
  const [deletePrefix, setDeletePrefix] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);

  const load = useCallback(async () => {
    try {
      const data = await listApiKeys();
      setKeys(data);
      setError("");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load API keys");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  async function handleCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!newKeyName.trim()) return;
    setCreating(true);
    try {
      const days = newKeyExpiry ? parseInt(newKeyExpiry, 10) : undefined;
      const result = await createApiKey(newKeyName.trim(), days);
      setCreatedKey(result);
      setCreateOpen(false);
      setNewKeyName("");
      setNewKeyExpiry("");
      load();
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to create API key",
      );
    } finally {
      setCreating(false);
    }
  }

  async function handleDelete(prefix: string) {
    setDeleting(true);
    try {
      await deleteApiKey(prefix);
      setDeletePrefix(null);
      load();
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to delete API key",
      );
    } finally {
      setDeleting(false);
    }
  }

  function handleCopy(text: string) {
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
        <p className="text-sm text-muted-foreground">
          Manage your API keys for programmatic access
        </p>
      </div>

      {error && (
        <div className="rounded-md border border-destructive/50 bg-destructive/10 px-4 py-3 text-sm text-destructive">
          {error}
        </div>
      )}

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="text-base">API Keys</CardTitle>
          <Dialog open={createOpen} onOpenChange={setCreateOpen}>
            <DialogTrigger asChild>
              <Button size="sm">
                <Plus className="mr-1 h-4 w-4" />
                Create API Key
              </Button>
            </DialogTrigger>
            <DialogContent>
              <form onSubmit={handleCreate}>
                <DialogHeader>
                  <DialogTitle>Create API Key</DialogTitle>
                  <DialogDescription>
                    Create a new API key for programmatic access. The key will
                    only be shown once.
                  </DialogDescription>
                </DialogHeader>
                <div className="grid gap-4 py-4">
                  <div className="grid gap-2">
                    <Label htmlFor="key-name">Name</Label>
                    <Input
                      id="key-name"
                      placeholder="e.g. CI/CD Pipeline"
                      value={newKeyName}
                      onChange={(e) => setNewKeyName(e.target.value)}
                      required
                    />
                  </div>
                  <div className="grid gap-2">
                    <Label htmlFor="key-expiry">
                      Expires in (days){" "}
                      <span className="text-muted-foreground font-normal">
                        - optional
                      </span>
                    </Label>
                    <Input
                      id="key-expiry"
                      type="number"
                      min="1"
                      placeholder="Leave empty for no expiration"
                      value={newKeyExpiry}
                      onChange={(e) => setNewKeyExpiry(e.target.value)}
                    />
                  </div>
                </div>
                <DialogFooter>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => setCreateOpen(false)}
                  >
                    Cancel
                  </Button>
                  <Button type="submit" disabled={creating || !newKeyName.trim()}>
                    {creating ? "Creating..." : "Create"}
                  </Button>
                </DialogFooter>
              </form>
            </DialogContent>
          </Dialog>
        </CardHeader>
        <CardContent>
          {/* Reveal key dialog */}
          <Dialog
            open={!!createdKey}
            onOpenChange={(open) => {
              if (!open) {
                setCreatedKey(null);
                setCopied(false);
              }
            }}
          >
            <DialogContent>
              <DialogHeader>
                <DialogTitle>API Key Created</DialogTitle>
                <DialogDescription>
                  Copy your API key now. You won't be able to see it again.
                </DialogDescription>
              </DialogHeader>
              {createdKey && (
                <div className="space-y-3">
                  <div className="flex items-center gap-2">
                    <code className="flex-1 rounded-md bg-muted px-3 py-2 text-sm font-mono break-all">
                      {createdKey.key}
                    </code>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => handleCopy(createdKey.key)}
                    >
                      <Copy className="h-4 w-4" />
                      {copied ? "Copied" : "Copy"}
                    </Button>
                  </div>
                  <p className="text-xs text-muted-foreground">
                    Use this key as a Bearer token:{" "}
                    <code className="rounded bg-muted px-1">
                      Authorization: Bearer {createdKey.prefix}...
                    </code>
                  </p>
                </div>
              )}
              <DialogFooter>
                <Button
                  onClick={() => {
                    setCreatedKey(null);
                    setCopied(false);
                  }}
                >
                  Done
                </Button>
              </DialogFooter>
            </DialogContent>
          </Dialog>

          {/* Delete confirmation dialog */}
          <Dialog
            open={!!deletePrefix}
            onOpenChange={(open) => {
              if (!open) setDeletePrefix(null);
            }}
          >
            <DialogContent>
              <DialogHeader>
                <DialogTitle>Revoke API Key</DialogTitle>
                <DialogDescription>
                  Are you sure you want to revoke this API key? Any applications
                  using it will lose access immediately.
                </DialogDescription>
              </DialogHeader>
              <DialogFooter>
                <Button
                  variant="outline"
                  onClick={() => setDeletePrefix(null)}
                >
                  Cancel
                </Button>
                <Button
                  variant="destructive"
                  disabled={deleting}
                  onClick={() => deletePrefix && handleDelete(deletePrefix)}
                >
                  {deleting ? "Revoking..." : "Revoke"}
                </Button>
              </DialogFooter>
            </DialogContent>
          </Dialog>

          {loading ? (
            <div className="flex items-center justify-center py-12">
              <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
            </div>
          ) : keys.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No API keys yet. Create one to get started.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Key</TableHead>
                  <TableHead>Created</TableHead>
                  <TableHead>Expires</TableHead>
                  <TableHead>Last Used</TableHead>
                  <TableHead className="w-[50px]" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {keys.map((k) => (
                  <TableRow key={k.prefix}>
                    <TableCell className="font-medium">{k.name}</TableCell>
                    <TableCell>
                      <code className="rounded bg-muted px-1.5 py-0.5 text-xs font-mono">
                        {k.prefix}...
                      </code>
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatTime(k.created_at)}
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {k.expires_at ? (
                        isExpired(k.expires_at) ? (
                          <Badge variant="destructive" className="text-xs">
                            Expired
                          </Badge>
                        ) : (
                          formatTime(k.expires_at)
                        )
                      ) : (
                        <span className="text-muted-foreground">Never</span>
                      )}
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {k.last_used_at ? formatTime(k.last_used_at) : "Never"}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => setDeletePrefix(k.prefix)}
                      >
                        <Trash2 className="h-4 w-4 text-muted-foreground" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
