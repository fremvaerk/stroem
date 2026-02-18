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
} from "./types";

let accessToken: string | null = null;
let refreshPromise: Promise<boolean> | null = null;

export function setAccessToken(token: string | null) {
  accessToken = token;
}

export function getAccessToken(): string | null {
  return accessToken;
}

function getRefreshToken(): string | null {
  return localStorage.getItem("stroem_refresh_token");
}

export function hasRefreshToken(): boolean {
  return !!getRefreshToken();
}

function setRefreshToken(token: string | null) {
  if (token) {
    localStorage.setItem("stroem_refresh_token", token);
  } else {
    localStorage.removeItem("stroem_refresh_token");
  }
}

async function refreshAccessToken(): Promise<boolean> {
  const refreshToken = getRefreshToken();
  if (!refreshToken) return false;

  try {
    const res = await fetch("/api/auth/refresh", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ refresh_token: refreshToken }),
    });
    if (!res.ok) {
      setRefreshToken(null);
      setAccessToken(null);
      return false;
    }
    const data: TokenResponse = await res.json();
    setAccessToken(data.access_token);
    setRefreshToken(data.refresh_token);
    return true;
  } catch {
    setRefreshToken(null);
    setAccessToken(null);
    return false;
  }
}

async function apiFetch<T>(
  url: string,
  options: RequestInit = {},
): Promise<T> {
  // Preemptively refresh if we have no access token but have a refresh token
  if (!accessToken && getRefreshToken()) {
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

  if (res.status === 401 && getRefreshToken()) {
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

  return res.json();
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
  const res = await fetch("/api/auth/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new ApiError(res.status, body.error || "Login failed");
  }
  const data: TokenResponse = await res.json();
  setAccessToken(data.access_token);
  setRefreshToken(data.refresh_token);
  return data;
}

export async function logout(): Promise<void> {
  const refreshToken = getRefreshToken();
  if (refreshToken) {
    await fetch("/api/auth/logout", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ refresh_token: refreshToken }),
    }).catch(() => {});
  }
  setAccessToken(null);
  setRefreshToken(null);
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
}

export async function getServerConfig(): Promise<ServerConfig> {
  try {
    const res = await fetch("/api/config");
    if (!res.ok)
      return { authRequired: false, hasInternalAuth: false, oidcProviders: [] };
    const data = await res.json();
    return {
      authRequired: !!data.auth_required,
      hasInternalAuth: !!data.has_internal_auth,
      oidcProviders: data.oidc_providers || [],
    };
  } catch {
    return { authRequired: false, hasInternalAuth: false, oidcProviders: [] };
  }
}

export function setTokensFromOidc(accessToken: string, refreshToken: string) {
  setAccessToken(accessToken);
  setRefreshToken(refreshToken);
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

// Jobs
export async function listJobs(
  limit = 50,
  offset = 0,
  filters?: { workspace?: string; taskName?: string },
): Promise<JobListItem[]> {
  const params = new URLSearchParams({
    limit: String(limit),
    offset: String(offset),
  });
  if (filters?.workspace) params.set("workspace", filters.workspace);
  if (filters?.taskName) params.set("task_name", filters.taskName);
  return apiFetch<JobListItem[]>(`/api/jobs?${params}`);
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
): Promise<WorkerListItem[]> {
  return apiFetch<WorkerListItem[]>(
    `/api/workers?limit=${limit}&offset=${offset}`,
  );
}

// Users
export async function listUsers(
  limit = 50,
  offset = 0,
): Promise<UserListItem[]> {
  return apiFetch<UserListItem[]>(
    `/api/users?limit=${limit}&offset=${offset}`,
  );
}

export async function getUser(id: string): Promise<UserDetail> {
  return apiFetch<UserDetail>(`/api/users/${id}`);
}

export async function getStepLogs(
  jobId: string,
  stepName: string,
): Promise<{ logs: string }> {
  return apiFetch<{ logs: string }>(
    `/api/jobs/${jobId}/steps/${encodeURIComponent(stepName)}/logs`,
  );
}
