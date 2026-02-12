import { Navigate, Outlet } from "react-router";
import { useAuth } from "@/context/auth-context";

export function ProtectedRoute() {
  const { authRequired, isAuthenticated, isLoading } = useAuth();

  if (isLoading) {
    return (
      <div className="flex h-screen items-center justify-center">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  if (authRequired && !isAuthenticated) {
    return <Navigate to="/login" replace />;
  }

  return <Outlet />;
}
