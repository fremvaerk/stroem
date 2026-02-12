export interface TaskListItem {
  name: string;
  mode: string;
}

export interface InputField {
  type: string;
  description?: string;
  default?: unknown;
  required?: boolean;
}

export interface FlowStep {
  action: string;
  input?: Record<string, unknown>;
  depends_on?: string[];
  continue_on_failure?: boolean;
}

export interface TaskDetail {
  name: string;
  mode: string;
  input: Record<string, InputField>;
  flow: Record<string, FlowStep>;
}

export interface JobListItem {
  job_id: string;
  workspace: string;
  task_name: string;
  mode: string;
  status: string;
  source_type: string;
  source_id: string | null;
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
}

export interface JobStep {
  step_name: string;
  action_name: string;
  action_type: string;
  action_image: string | null;
  input: Record<string, unknown> | null;
  output: Record<string, unknown> | null;
  status: string;
  worker_id: string | null;
  started_at: string | null;
  completed_at: string | null;
  error_message: string | null;
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
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  steps: JobStep[];
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
