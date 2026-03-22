import { Routes, Route } from "react-router";
import { AuthProvider } from "@/context/auth-context";
import { ErrorBoundary } from "@/components/error-boundary";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ProtectedRoute } from "@/components/protected-route";
import { AppLayout } from "@/components/app-layout";
import { LoginPage } from "@/pages/login";
import { DashboardPage } from "@/pages/dashboard";
import { TasksPage } from "@/pages/tasks";
import { TaskDetailPage } from "@/pages/task-detail";
import { JobsPage } from "@/pages/jobs";
import { JobDetailPage } from "@/pages/job-detail";
import { WorkspacesPage } from "@/pages/workspaces";
import { WorkersPage } from "@/pages/workers";
import { WorkerDetailPage } from "@/pages/worker-detail";
import { UsersPage } from "@/pages/users";
import { UserDetailPage } from "@/pages/user-detail";
import { LoginCallbackPage } from "@/pages/login-callback";
import { SettingsPage } from "@/pages/settings";
import { ApprovalPage } from "@/pages/approval";

export default function App() {
  return (
    <AuthProvider>
      <TooltipProvider>
        <ErrorBoundary>
          <Routes>
            <Route path="/login" element={<LoginPage />} />
            <Route path="/login/callback" element={<LoginCallbackPage />} />
            <Route element={<ProtectedRoute />}>
              <Route element={<AppLayout />}>
                <Route index element={<ErrorBoundary><DashboardPage /></ErrorBoundary>} />
                <Route path="workspaces" element={<ErrorBoundary><WorkspacesPage /></ErrorBoundary>} />
                <Route path="tasks" element={<ErrorBoundary><TasksPage /></ErrorBoundary>} />
                <Route
                  path="workspaces/:workspace/tasks/:name"
                  element={<ErrorBoundary><TaskDetailPage /></ErrorBoundary>}
                />
                <Route path="jobs" element={<ErrorBoundary><JobsPage /></ErrorBoundary>} />
                <Route path="jobs/:id" element={<ErrorBoundary><JobDetailPage /></ErrorBoundary>} />
                <Route path="approve/:jobId/:stepName" element={<ErrorBoundary><ApprovalPage /></ErrorBoundary>} />
                <Route path="workers" element={<ErrorBoundary><WorkersPage /></ErrorBoundary>} />
                <Route path="workers/:id" element={<ErrorBoundary><WorkerDetailPage /></ErrorBoundary>} />
                <Route path="users" element={<ErrorBoundary><UsersPage /></ErrorBoundary>} />
                <Route path="users/:id" element={<ErrorBoundary><UserDetailPage /></ErrorBoundary>} />
                <Route path="settings" element={<ErrorBoundary><SettingsPage /></ErrorBoundary>} />
              </Route>
            </Route>
          </Routes>
        </ErrorBoundary>
      </TooltipProvider>
    </AuthProvider>
  );
}
