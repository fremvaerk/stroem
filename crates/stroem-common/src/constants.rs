/// Default runner image used when no image is specified for shell-in-container or
/// shell-in-pod execution (script actions with `runner: docker` or `runner: pod`).
pub const DEFAULT_RUNNER_IMAGE: &str = "ghcr.io/fremvaerk/stroem-runner:latest";

/// Default init container image used to download workspace tarballs into pods.
pub const DEFAULT_INIT_IMAGE: &str = "curlimages/curl:latest";
