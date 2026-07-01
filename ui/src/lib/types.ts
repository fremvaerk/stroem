export interface WorkspaceInfo {
  name: string;
  tasks_count: number;
  actions_count: number;
  triggers_count: number;
  revision?: string;
  error?: string;
  warnings?: string[];
}

export interface TaskListItem {
  id: string;
  name?: string;
  description?: string;
  mode: string;
  workspace: string;
  folder?: string;
  has_triggers: boolean;
  can_execute?: boolean;
}

export interface InputField {
  type: string;
  name?: string;
  description?: string;
  default?: unknown;
  required?: boolean;
  secret?: boolean;
  options?: string[];
  allow_custom?: boolean;
  multiple?: boolean;
  order?: number;
}

export interface FlowStep {
  action: string;
  name?: string;
  description?: string;
  input?: Record<string, unknown>;
  depends_on?: string[];
  continue_on_failure?: boolean;
  when?: string;
  for_each?: string | string[] | unknown[];
  sequential?: boolean;
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
  connections?: Record<string, string[]>;
  can_execute?: boolean;
}

export interface JobListItem {
  job_id: string;
  workspace: string;
  task_name: string;
  mode: string;
  status: string;
  source_type: string;
  source_id: string | null;
  revision: string | null;
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
  suspended_at: string | null;
  error_message: string | null;
  when_condition: string | null;
  depends_on: string[];
  for_each_expr: string | null;
  loop_source: string | null;
  loop_index: number | null;
  loop_total: number | null;
  retry_attempt: number;
  max_retries: number | null;
  retry_history: Array<{
    attempt: number;
    error: string | null;
    started_at: string | null;
    failed_at: string | null;
  }>;
  retry_at: string | null;
  approval_message: string | null;
  approval_fields: Record<string, unknown> | null;
}

export interface JobDetail {
  job_id: string;
  workspace: string;
  task_name: string;
  mode: string;
  input: Record<string, unknown> | null;
  raw_input: Record<string, unknown> | null;
  output: Record<string, unknown> | null;
  status: string;
  source_type: string;
  source_id: string | null;
  source_job_id: string | null;
  restart_from_step: string | null;
  revision: string | null;
  worker_id: string | null;
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  retry_of_job_id: string | null;
  retry_job_id: string | null;
  retry_attempt: number;
  max_retries: number | null;
  steps: JobStep[];
}

export interface PaginatedResponse<T> {
  items: T[];
  total: number;
}

export interface RecentDuration {
  job_id: string;
  duration_ms: number;
  completed_at: string;
}

export interface TaskDurationStats {
  sample_size: number;
  avg_ms: number | null;
  p50_ms: number | null;
  p95_ms: number | null;
  min_ms: number | null;
  max_ms: number | null;
  recent: RecentDuration[];
}

export interface StepDurationStats {
  step_name: string;
  sample_size: number;
  avg_ms: number | null;
  p50_ms: number | null;
  p95_ms: number | null;
  min_ms: number | null;
  max_ms: number | null;
}

export interface TaskStatsResponse {
  window: number;
  task: TaskDurationStats;
  steps: StepDurationStats[];
}

export interface WorkerListItem {
  worker_id: string;
  name: string;
  status: string;
  /**
   * Runner types this worker supports — one or more of "script",
   * "docker", "kubernetes", "agent". Introduced by migration 041; see
   * `docs/operations/recovery.md`.
   */
  capabilities: string[];
  /**
   * Reservation labels (taints). Empty = permissive. Non-empty = worker
   * ONLY claims steps whose action tags include ALL of these labels.
   */
  tags: string[];
  version: string | null;
  last_heartbeat: string | null;
  registered_at: string;
}

export interface WorkerStepItem {
  job_id: string;
  workspace: string;
  task_name: string;
  job_status: string;
  step_name: string;
  action_type: string;
  status: string;
  started_at: string | null;
  completed_at: string | null;
  error_message: string | null;
}

export interface WorkerDetail extends WorkerListItem {
  steps: PaginatedResponse<WorkerStepItem>;
}

export interface UserListItem {
  user_id: string;
  name: string | null;
  email: string;
  auth_methods: string[];
  created_at: string;
  last_login_at: string | null;
  is_admin?: boolean;
  groups?: string[];
}

export interface UserDetail {
  user_id: string;
  name: string | null;
  email: string;
  auth_methods: string[];
  created_at: string;
  last_login_at: string | null;
  is_admin?: boolean;
  groups?: string[];
}

export interface TokenResponse {
  access_token: string;
}

export interface AuthUser {
  user_id: string;
  name: string;
  email: string;
  created_at: string;
  is_admin?: boolean;
  groups?: string[];
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
