import { Routes, Route } from "react-router";
import { AuthProvider } from "@/context/auth-context";
import { ProtectedRoute } from "@/components/protected-route";
import { AppLayout } from "@/components/app-layout";
import { LoginPage } from "@/pages/login";
import { DashboardPage } from "@/pages/dashboard";
import { TasksPage } from "@/pages/tasks";
import { TaskDetailPage } from "@/pages/task-detail";
import { JobsPage } from "@/pages/jobs";
import { JobDetailPage } from "@/pages/job-detail";
import { TaskRunPage } from "@/pages/task-run";
import { WorkspacesPage } from "@/pages/workspaces";
import { WorkersPage } from "@/pages/workers";
import { LoginCallbackPage } from "@/pages/login-callback";

export default function App() {
  return (
    <AuthProvider>
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route path="/login/callback" element={<LoginCallbackPage />} />
        <Route element={<ProtectedRoute />}>
          <Route element={<AppLayout />}>
            <Route index element={<DashboardPage />} />
            <Route path="workspaces" element={<WorkspacesPage />} />
            <Route path="tasks" element={<TasksPage />} />
            <Route
              path="workspaces/:workspace/tasks/:name"
              element={<TaskDetailPage />}
            />
            <Route
              path="workspaces/:workspace/tasks/:name/run"
              element={<TaskRunPage />}
            />
            <Route path="jobs" element={<JobsPage />} />
            <Route path="jobs/:id" element={<JobDetailPage />} />
            <Route path="workers" element={<WorkersPage />} />
          </Route>
        </Route>
      </Routes>
    </AuthProvider>
  );
}
