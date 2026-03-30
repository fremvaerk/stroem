group "default" {
  targets = ["server", "worker"]
}

target "server" {
  context    = "."
  dockerfile = "Dockerfile.server"
  tags       = ["stroem-server:ci"]
  cache-from = ["type=gha,scope=stroem-server"]
  cache-to   = ["type=gha,scope=stroem-server,mode=max"]
}

target "worker" {
  context    = "."
  dockerfile = "Dockerfile.worker"
  tags       = ["stroem-worker:ci"]
  cache-from = ["type=gha,scope=stroem-worker"]
  cache-to   = ["type=gha,scope=stroem-worker,mode=max"]
}
