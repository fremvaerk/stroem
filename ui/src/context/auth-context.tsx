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
  serverVersion: string | null;
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
  const [serverVersion, setServerVersion] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function init() {
      const config = await api.getServerConfig();
      if (cancelled) return;
      setAuthRequired(config.authRequired);
      setHasInternalAuth(config.hasInternalAuth);
      setOidcProviders(config.oidcProviders);
      setServerVersion(config.version);

      if (config.authRequired) {
        try {
          // tryRestoreSession attempts a silent token refresh using the
          // HttpOnly refresh cookie. If the cookie is absent or expired this
          // resolves false and we stay unauthenticated.
          const restored = await api.tryRestoreSession();
          if (restored && !cancelled) {
            const me = await api.getMe();
            if (!cancelled) setUser(me);
          }
        } catch {
          // Refresh token expired or invalid — stay logged out
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
      serverVersion,
      login,
      logout,
    }),
    [user, isLoading, authRequired, hasInternalAuth, oidcProviders, serverVersion, login, logout],
  );

  return <AuthContext value={value}>{children}</AuthContext>;
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within AuthProvider");
  return ctx;
}
