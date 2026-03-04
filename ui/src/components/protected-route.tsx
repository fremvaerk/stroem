import { Navigate, Outlet } from "react-router";
import { useAuth } from "@/context/auth-context";
import { LoadingSpinner } from "@/components/loading-spinner";

export function ProtectedRoute() {
  const { authRequired, isAuthenticated, isLoading } = useAuth();

  if (isLoading) {
    return <LoadingSpinner className="flex h-screen items-center justify-center" />;
  }

  if (authRequired && !isAuthenticated) {
    return <Navigate to="/login" replace />;
  }

  return <Outlet />;
}
