---
title: Installation
description: How to install Strøm server, worker, and CLI
---

Strøm ships three binaries: `stroem-server`, `stroem-worker`, and `stroem` (CLI). You can install them via pre-built binaries or Docker images.

## Pre-built binaries

Download the latest release from [GitHub Releases](https://github.com/fremvaerk/stroem/releases). Binaries are available for:

| Platform | Archive |
|----------|---------|
| Linux (x86_64) | `stroem-{server,worker,cli}-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (ARM64) | `stroem-{server,worker,cli}-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `stroem-{server,worker,cli}-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `stroem-{server,worker,cli}-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `stroem-{server,worker,cli}-x86_64-pc-windows-msvc.zip` |

```bash
# Example: install the CLI on macOS ARM64
curl -fsSL https://github.com/fremvaerk/stroem/releases/latest/download/stroem-cli-aarch64-apple-darwin.tar.gz \
  | tar xz -C /usr/local/bin
```

## Docker images

Multi-arch images (amd64 + arm64) are published to GHCR:

```bash
docker pull ghcr.io/fremvaerk/stroem-server:latest
docker pull ghcr.io/fremvaerk/stroem-worker:latest
docker pull ghcr.io/fremvaerk/stroem-runner:latest  # base image for shell-in-container steps
```

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
