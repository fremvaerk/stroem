import type {
  WorkspaceInfo,
  TaskListItem,
  TaskDetail,
  TriggerInfo,
  JobListItem,
  JobDetail,
  WorkerListItem,
  WorkerDetail,
  UserListItem,
  UserDetail,
  TokenResponse,
  AuthUser,
  ExecuteTaskResponse,
  ApiKey,
  CreateApiKeyResponse,
} from "./types";

let accessToken: string | null = null;
let refreshPromise: Promise<boolean> | null = null;

export function setAccessToken(token: string | null) {
  accessToken = token;
}

export function getAccessToken(): string | null {
  return accessToken;
}

// The refresh token is stored in an HttpOnly cookie managed by the server.
// The browser sends it automatically on requests to /api/auth/* when
// credentials: "include" is set — JavaScript cannot read or write it.

async function refreshAccessToken(): Promise<boolean> {
  try {
    const res = await fetch("/api/auth/refresh", {
      method: "POST",
      credentials: "include",
    });
    if (!res.ok) {
      setAccessToken(null);
      return false;
    }
    const data: TokenResponse = await res.json();
    setAccessToken(data.access_token);
    return true;
  } catch {
    setAccessToken(null);
    return false;
  }
}

// Attempt a silent refresh on startup to check whether a valid refresh cookie
// exists. Used by the auth context to restore session across page reloads.
export async function tryRestoreSession(): Promise<boolean> {
  return refreshAccessToken();
}

async function apiFetch<T>(
  url: string,
  options: RequestInit = {},
): Promise<T> {
  // Preemptively refresh if we have no access token (cookie may still be valid)
  if (!accessToken) {
    if (!refreshPromise) {
      refreshPromise = refreshAccessToken().finally(() => {
        refreshPromise = null;
      });
    }
    await refreshPromise;
  }

  const headers: Record<string, string> = {
    ...(options.headers as Record<string, string>),
  };

  if (accessToken) {
    headers["Authorization"] = `Bearer ${accessToken}`;
  }

  if (options.body && !headers["Content-Type"]) {
    headers["Content-Type"] = "application/json";
  }

  let res = await fetch(url, { ...options, headers });

  if (res.status === 401) {
    if (!refreshPromise) {
      refreshPromise = refreshAccessToken().finally(() => {
        refreshPromise = null;
      });
    }
    const refreshed = await refreshPromise;
    if (refreshed) {
      headers["Authorization"] = `Bearer ${accessToken}`;
      res = await fetch(url, { ...options, headers });
    }
  }

  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new ApiError(res.status, body.error || res.statusText);
  }

  if (res.status === 204 || res.headers.get("content-length") === "0") {
    return null as T;
  }

  return res.json();
}

export interface PaginatedResponse<T> {
  items: T[];
  total: number;
}

export class ApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

// Auth
export async function login(
  email: string,
  password: string,
): Promise<TokenResponse> {
  // credentials: "include" ensures the Set-Cookie response header is accepted
  // by the browser (the refresh token HttpOnly cookie).
  const res = await fetch("/api/auth/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ email, password }),
    credentials: "include",
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new ApiError(res.status, body.error || "Login failed");
  }
  const data: TokenResponse = await res.json();
  setAccessToken(data.access_token);
  return data;
}

export async function logout(): Promise<void> {
  // credentials: "include" sends the refresh cookie so the server can revoke it,
  // and the response clears the cookie (Max-Age=0).
  await fetch("/api/auth/logout", {
    method: "POST",
    credentials: "include",
  }).catch(() => {});
  setAccessToken(null);
}

export async function getMe(): Promise<AuthUser> {
  return apiFetch<AuthUser>("/api/auth/me");
}

export interface OidcProvider {
  id: string;
  display_name: string;
}

export interface ServerConfig {
  authRequired: boolean;
  hasInternalAuth: boolean;
  oidcProviders: OidcProvider[];
  version: string | null;
}

export async function getServerConfig(): Promise<ServerConfig> {
  try {
    const res = await fetch("/api/config");
    if (!res.ok)
      return { authRequired: false, hasInternalAuth: false, oidcProviders: [], version: null };
    const data = await res.json();
    return {
      authRequired: !!data.auth_required,
      hasInternalAuth: !!data.has_internal_auth,
      oidcProviders: data.oidc_providers || [],
      version: data.version ?? null,
    };
  } catch {
    return { authRequired: false, hasInternalAuth: false, oidcProviders: [], version: null };
  }
}

export function setTokensFromOidc(accessToken: string) {
  setAccessToken(accessToken);
  // The refresh token arrives as an HttpOnly cookie set by the OIDC callback
  // redirect — no JavaScript action required.
}

// Workspaces
export async function listWorkspaces(): Promise<WorkspaceInfo[]> {
  return apiFetch<WorkspaceInfo[]>("/api/workspaces");
}

// Tasks
export async function listTasks(workspace: string): Promise<TaskListItem[]> {
  return apiFetch<TaskListItem[]>(
    `/api/workspaces/${encodeURIComponent(workspace)}/tasks`,
  );
}

export async function listAllTasks(): Promise<TaskListItem[]> {
  const workspaces = await listWorkspaces();
  const results = await Promise.all(
    workspaces.map((ws) => listTasks(ws.name)),
  );
  return results.flat();
}

export async function getTask(
  workspace: string,
  name: string,
): Promise<TaskDetail> {
  return apiFetch<TaskDetail>(
    `/api/workspaces/${encodeURIComponent(workspace)}/tasks/${encodeURIComponent(name)}`,
  );
}

export async function executeTask(
  workspace: string,
  name: string,
  input: Record<string, unknown>,
): Promise<ExecuteTaskResponse> {
  return apiFetch<ExecuteTaskResponse>(
    `/api/workspaces/${encodeURIComponent(workspace)}/tasks/${encodeURIComponent(name)}/execute`,
    {
      method: "POST",
      body: JSON.stringify({ input }),
    },
  );
}

// Triggers
export async function listTriggers(
  workspace: string,
): Promise<TriggerInfo[]> {
  return apiFetch<TriggerInfo[]>(
    `/api/workspaces/${encodeURIComponent(workspace)}/triggers`,
  );
}

// Stats
export interface DashboardStats {
  pending: number;
  running: number;
  completed: number;
  failed: number;
  cancelled: number;
}

export async function getStats(): Promise<DashboardStats> {
  return apiFetch<DashboardStats>("/api/stats");
}

// Jobs
export async function listJobs(
  limit = 50,
  offset = 0,
  filters?: { workspace?: string; taskName?: string; status?: string },
): Promise<PaginatedResponse<JobListItem>> {
  const params = new URLSearchParams({
    limit: String(limit),
    offset: String(offset),
  });
  if (filters?.workspace) params.set("workspace", filters.workspace);
  if (filters?.taskName) params.set("task_name", filters.taskName);
  if (filters?.status) params.set("status", filters.status);
  return apiFetch<PaginatedResponse<JobListItem>>(`/api/jobs?${params}`);
}

export async function getJob(id: string): Promise<JobDetail> {
  return apiFetch<JobDetail>(`/api/jobs/${id}`);
}

export async function getJobLogs(
  id: string,
): Promise<{ logs: string }> {
  return apiFetch<{ logs: string }>(`/api/jobs/${id}/logs`);
}

// Workers
export async function getWorker(id: string): Promise<WorkerDetail> {
  return apiFetch<WorkerDetail>(`/api/workers/${id}`);
}

export async function listWorkers(
  limit = 50,
  offset = 0,
): Promise<PaginatedResponse<WorkerListItem>> {
  return apiFetch<PaginatedResponse<WorkerListItem>>(
    `/api/workers?limit=${limit}&offset=${offset}`,
  );
}

// Users
export async function listUsers(
  limit = 50,
  offset = 0,
): Promise<PaginatedResponse<UserListItem>> {
  return apiFetch<PaginatedResponse<UserListItem>>(
    `/api/users?limit=${limit}&offset=${offset}`,
  );
}

export async function getUser(id: string): Promise<UserDetail> {
  return apiFetch<UserDetail>(`/api/users/${id}`);
}

export async function cancelJob(id: string): Promise<{ status: string }> {
  return apiFetch<{ status: string }>(`/api/jobs/${encodeURIComponent(id)}/cancel`, {
    method: "POST",
  });
}

export async function getStepLogs(
  jobId: string,
  stepName: string,
): Promise<{ logs: string }> {
  return apiFetch<{ logs: string }>(
    `/api/jobs/${jobId}/steps/${encodeURIComponent(stepName)}/logs`,
  );
}

// API Keys
export async function listApiKeys(): Promise<ApiKey[]> {
  return apiFetch<ApiKey[]>("/api/auth/api-keys");
}

export async function createApiKey(
  name: string,
  expiresInDays?: number,
): Promise<CreateApiKeyResponse> {
  return apiFetch<CreateApiKeyResponse>("/api/auth/api-keys", {
    method: "POST",
    body: JSON.stringify({
      name,
      expires_in_days: expiresInDays ?? null,
    }),
  });
}

export async function deleteApiKey(prefix: string): Promise<void> {
  await apiFetch<{ status: string }>(
    `/api/auth/api-keys/${encodeURIComponent(prefix)}`,
    { method: "DELETE" },
  );
}
