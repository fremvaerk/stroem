---
title: Installation
description: How to install Strøm server, worker, and CLI
---

Strøm ships four binaries: `stroem-server`, `stroem-worker`, `stroem` (local workspace CLI) and `stroem-api` (remote server client). You can install them via pre-built binaries or Docker images.

## Pre-built binaries

Download the latest release from [GitHub Releases](https://github.com/fremvaerk/stroem/releases). Each release has one archive per binary and Linux target:

| Platform | Archives |
|----------|----------|
| Linux (x86_64) | `stroem-server-x86_64-unknown-linux-gnu.tar.gz`, `stroem-worker-x86_64-unknown-linux-gnu.tar.gz`, `stroem-x86_64-unknown-linux-gnu.tar.gz`, `stroem-api-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (ARM64) | `stroem-server-aarch64-unknown-linux-gnu.tar.gz`, `stroem-worker-aarch64-unknown-linux-gnu.tar.gz`, `stroem-aarch64-unknown-linux-gnu.tar.gz`, `stroem-api-aarch64-unknown-linux-gnu.tar.gz` |

`stroem` is the local workspace CLI and `stroem-api` the remote server client. The binaries are built on Debian 12 (glibc 2.36) and run on Linux distributions with that glibc or a newer one. macOS and Windows binaries are not published; build those from source.

```bash
# Example: install the stroem CLI on Linux
ARCH=$(uname -m)   # x86_64 or aarch64
curl -fsSL "https://github.com/fremvaerk/stroem/releases/latest/download/stroem-${ARCH}-unknown-linux-gnu.tar.gz" \
  | sudo tar xz -C /usr/local/bin
```

## Docker images

Images are published to GHCR. Release tags (`X.Y.Z`, `X.Y`, `X`) are multi-arch (`linux/amd64` + `linux/arm64`):

```bash
docker pull ghcr.io/fremvaerk/stroem-server:0.18
docker pull ghcr.io/fremvaerk/stroem-worker:0.18
docker pull ghcr.io/fremvaerk/stroem-runner:0.18  # base image for script-in-container steps
```

The `latest` and `sha-<commit>` tags are built from `main` and are `linux/amd64` only. Releases up to v0.18.3 have `linux/amd64` images and x86_64 archives only.

## Build from source

Prerequisites: Rust (latest stable), Docker (for integration tests).

```bash
git clone https://github.com/fremvaerk/stroem.git
cd stroem
cargo build --workspace
```

The binaries are placed in `target/debug/` (or `target/release/` with `--release`).

### Feature flags

The worker binary supports optional features for container runners:

```bash
# Docker runner only
cargo build -p stroem-worker --features docker

# Kubernetes runner only
cargo build -p stroem-worker --features kubernetes

# Both (default when building the full workspace)
cargo build -p stroem-worker --features docker,kubernetes
```

## Requirements

- **Server**: PostgreSQL 14+ database
- **Worker**: Network access to the server. Optionally Docker daemon access (for Docker runner) or Kubernetes API access (for Kubernetes runner).
- **CLI**: Network access to the server
