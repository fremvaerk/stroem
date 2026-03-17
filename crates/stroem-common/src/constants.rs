/// Default runner image used when no image is specified for shell-in-container or
/// shell-in-pod execution (Type 2 runner mode).
pub const DEFAULT_RUNNER_IMAGE: &str = "ghcr.io/fremvaerk/stroem-runner:latest";

/// Default init container image used to download workspace tarballs into pods.
pub const DEFAULT_INIT_IMAGE: &str = "curlimages/curl:latest";
