pub mod shell;
pub mod traits;

#[cfg(feature = "docker")]
#[allow(deprecated)] // bollard 0.19 deprecates types; locked for testcontainers compat
pub mod docker;

#[cfg(feature = "kubernetes")]
pub mod kubernetes;

pub use shell::ShellRunner;
pub use traits::{LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner};

#[cfg(feature = "docker")]
pub use docker::DockerRunner;

#[cfg(feature = "kubernetes")]
pub use kubernetes::KubeRunner;
