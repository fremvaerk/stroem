{{/*
Validation: when replicas > 1 and auth is enabled, a pinned JWT secret must be
present. Without it every pod generates its own secret at startup and refresh
tokens minted on one pod fail validation on any other — silent split-brain on
every other login.

A secret is considered "pinned" when the operator provides it through any of
the supported paths:
  1. server.extraSecretEnv.STROEM__AUTH__JWT_SECRET  (recommended)
  2. server.config.auth.jwt_secret                   (less secure — ends up in
                                                       the ConfigMap plaintext)
*/}}
{{- define "stroem.validateHaAuth" -}}
{{- if gt (int .Values.server.replicas) 1 -}}
  {{- $authEnabled := and .Values.server.config.auth (not (empty .Values.server.config.auth)) -}}
  {{- if $authEnabled -}}
    {{- $pinnedViaEnv    := hasKey .Values.server.extraSecretEnv "STROEM__AUTH__JWT_SECRET" -}}
    {{- $pinnedViaConfig := and .Values.server.config.auth .Values.server.config.auth.jwt_secret -}}
    {{- if not (or $pinnedViaEnv $pinnedViaConfig) -}}
      {{- fail (printf "\n\nERROR: server.replicas=%d with auth enabled but no pinned JWT secret.\n\nAll server pods must share the same JWT signing key; without it refresh\ntokens minted on one pod are rejected by every other pod (split-brain).\n\nFix — add to your values file:\n\n  server:\n    extraSecretEnv:\n      STROEM__AUTH__JWT_SECRET: \"<your-secret>\"\n      STROEM__AUTH__REFRESH_SECRET: \"<your-refresh-secret>\"\n\nSee docs/operations/high-availability.md for details.\n" (int .Values.server.replicas)) -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- end -}}
