import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";
import type { AuthUser } from "@/lib/types";
import type { OidcProvider } from "@/lib/api";
import * as api from "@/lib/api";

interface AuthContextValue {
  user: AuthUser | null;
  isAuthenticated: boolean;
  isLoading: boolean;
  authRequired: boolean;
  hasInternalAuth: boolean;
  oidcProviders: OidcProvider[];
  login: (email: string, password: string) => Promise<void>;
  logout: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<AuthUser | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [authRequired, setAuthRequired] = useState(false);
  const [hasInternalAuth, setHasInternalAuth] = useState(false);
  const [oidcProviders, setOidcProviders] = useState<OidcProvider[]>([]);

  useEffect(() => {
    let cancelled = false;

    async function init() {
      const config = await api.getServerConfig();
      if (cancelled) return;
      setAuthRequired(config.authRequired);
      setHasInternalAuth(config.hasInternalAuth);
      setOidcProviders(config.oidcProviders);

      if (config.authRequired && api.hasRefreshToken()) {
        try {
          const me = await api.getMe();
          if (!cancelled) setUser(me);
        } catch {
          // Refresh token expired or invalid
        }
      }
      if (!cancelled) setIsLoading(false);
    }

    init();
    return () => {
      cancelled = true;
    };
  }, []);

  const login = useCallback(async (email: string, password: string) => {
    await api.login(email, password);
    const me = await api.getMe();
    setUser(me);
  }, []);

  const logout = useCallback(async () => {
    await api.logout();
    setUser(null);
  }, []);

  const value = useMemo(
    () => ({
      user,
      isAuthenticated: !!user,
      isLoading,
      authRequired,
      hasInternalAuth,
      oidcProviders,
      login,
      logout,
    }),
    [user, isLoading, authRequired, hasInternalAuth, oidcProviders, login, logout],
  );

  return <AuthContext value={value}>{children}</AuthContext>;
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within AuthProvider");
  return ctx;
}
