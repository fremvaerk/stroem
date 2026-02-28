export interface WorkspaceInfo {
  name: string;
  tasks_count: number;
  actions_count: number;
  triggers_count: number;
  revision?: string;
}

export interface TaskListItem {
  id: string;
  name?: string;
  description?: string;
  mode: string;
  workspace: string;
  folder?: string;
  has_triggers: boolean;
}

export interface InputField {
  type: string;
  name?: string;
  description?: string;
  default?: unknown;
  required?: boolean;
  secret?: boolean;
}

export interface FlowStep {
  action: string;
  name?: string;
  description?: string;
  input?: Record<string, unknown>;
  depends_on?: string[];
  continue_on_failure?: boolean;
}

export interface TriggerInfo {
  name: string;
  type: string;
  cron: string | null;
  task: string;
  enabled: boolean;
  input: Record<string, unknown>;
  next_runs: string[];
}

export interface TaskDetail {
  id: string;
  name?: string;
  description?: string;
  mode: string;
  folder?: string;
  input: Record<string, InputField>;
  flow: Record<string, FlowStep>;
  triggers: TriggerInfo[];
}

export interface JobListItem {
  job_id: string;
  workspace: string;
  task_name: string;
  mode: string;
  status: string;
  source_type: string;
  source_id: string | null;
  worker_id: string | null;
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
}

export interface JobStep {
  step_name: string;
  action_name: string;
  action_type: string;
  action_image: string | null;
  runner: string;
  input: Record<string, unknown> | null;
  output: Record<string, unknown> | null;
  status: string;
  worker_id: string | null;
  started_at: string | null;
  completed_at: string | null;
  error_message: string | null;
  depends_on: string[];
}

export interface JobDetail {
  job_id: string;
  workspace: string;
  task_name: string;
  mode: string;
  input: Record<string, unknown> | null;
  output: Record<string, unknown> | null;
  status: string;
  source_type: string;
  source_id: string | null;
  worker_id: string | null;
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  steps: JobStep[];
}

export interface WorkerListItem {
  worker_id: string;
  name: string;
  status: string;
  tags: string[];
  last_heartbeat: string | null;
  registered_at: string;
}

export interface WorkerDetail extends WorkerListItem {
  jobs: JobListItem[];
}

export interface UserListItem {
  user_id: string;
  name: string | null;
  email: string;
  auth_methods: string[];
  created_at: string;
  last_login_at: string | null;
}

export interface UserDetail {
  user_id: string;
  name: string | null;
  email: string;
  auth_methods: string[];
  created_at: string;
  last_login_at: string | null;
}

export interface TokenResponse {
  access_token: string;
  refresh_token: string;
}

export interface AuthUser {
  user_id: string;
  name: string;
  email: string;
  created_at: string;
}

export interface ExecuteTaskResponse {
  job_id: string;
}

export interface ApiKey {
  prefix: string;
  name: string;
  created_at: string;
  expires_at: string | null;
  last_used_at: string | null;
}

export interface CreateApiKeyResponse {
  key: string;
  name: string;
  prefix: string;
  expires_at: string | null;
}
