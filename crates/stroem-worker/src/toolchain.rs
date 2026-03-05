use stroem_common::language::ScriptLanguage;
use stroem_runner::script_exec;

/// Detected toolchain availability for each supported language.
pub struct Toolchains {
    pub shell: Option<String>,
    pub python: Option<String>,
    pub javascript: Option<String>,
    pub typescript: Option<String>,
    pub go: Option<String>,
}

impl Toolchains {
    /// Probe all supported languages and record which interpreters are available.
    pub fn detect() -> Self {
        Self {
            shell: Self::detect_lang(ScriptLanguage::Shell),
            python: Self::detect_lang(ScriptLanguage::Python),
            javascript: Self::detect_lang(ScriptLanguage::JavaScript),
            typescript: Self::detect_lang(ScriptLanguage::TypeScript),
            go: Self::detect_lang(ScriptLanguage::Go),
        }
    }

    fn detect_lang(lang: ScriptLanguage) -> Option<String> {
        script_exec::resolve_toolchain(lang, None)
            .ok()
            .map(|(binary, _)| binary)
    }

    /// Log a summary of available toolchains.
    pub fn log_summary(&self) {
        let entries = [
            ("shell", &self.shell),
            ("python", &self.python),
            ("javascript", &self.javascript),
            ("typescript", &self.typescript),
            ("go", &self.go),
        ];

        let mut available = Vec::new();
        let mut missing = Vec::new();

        for (name, binary) in &entries {
            match binary {
                Some(bin) => available.push(format!("{name}={bin}")),
                None => missing.push(*name),
            }
        }

        tracing::info!(
            "Toolchains available: {}",
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        );

        if !missing.is_empty() {
            tracing::debug!("Toolchains not found: {}", missing.join(", "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_finds_shell() {
        let toolchains = Toolchains::detect();
        // sh or bash should always be available
        assert!(toolchains.shell.is_some());
    }

    #[test]
    fn test_log_summary_does_not_panic() {
        let toolchains = Toolchains::detect();
        toolchains.log_summary();
    }
}
