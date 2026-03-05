use std::fmt;

/// Valid language names for `type: script` actions.
pub const VALID_LANGUAGE_NAMES: &[&str] = &[
    "shell",
    "python",
    "javascript",
    "typescript",
    "go",
    "js",
    "ts",
];

/// Script language for inline script execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptLanguage {
    Shell,
    Python,
    JavaScript,
    TypeScript,
    Go,
}

impl ScriptLanguage {
    /// Parse a language string, defaulting to Shell when absent.
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s {
            None | Some("shell") => Self::Shell,
            Some("python") => Self::Python,
            Some("javascript") | Some("js") => Self::JavaScript,
            Some("typescript") | Some("ts") => Self::TypeScript,
            Some("go") => Self::Go,
            Some(_) => Self::Shell,
        }
    }

    /// File extension for the script (including the dot).
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Shell => ".sh",
            Self::Python => ".py",
            Self::JavaScript => ".js",
            Self::TypeScript => ".ts",
            Self::Go => ".go",
        }
    }

    /// Whether this is the default shell language.
    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Shell)
    }

    /// The canonical name string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Go => "go",
        }
    }
}

impl fmt::Display for ScriptLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_str_opt_defaults_to_shell() {
        assert_eq!(ScriptLanguage::from_str_opt(None), ScriptLanguage::Shell);
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("shell")),
            ScriptLanguage::Shell
        );
    }

    #[test]
    fn test_from_str_opt_languages() {
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("python")),
            ScriptLanguage::Python
        );
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("javascript")),
            ScriptLanguage::JavaScript
        );
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("js")),
            ScriptLanguage::JavaScript
        );
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("typescript")),
            ScriptLanguage::TypeScript
        );
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("ts")),
            ScriptLanguage::TypeScript
        );
        assert_eq!(ScriptLanguage::from_str_opt(Some("go")), ScriptLanguage::Go);
    }

    #[test]
    fn test_from_str_opt_unknown_defaults_to_shell() {
        assert_eq!(
            ScriptLanguage::from_str_opt(Some("rust")),
            ScriptLanguage::Shell
        );
    }

    #[test]
    fn test_extensions() {
        assert_eq!(ScriptLanguage::Shell.extension(), ".sh");
        assert_eq!(ScriptLanguage::Python.extension(), ".py");
        assert_eq!(ScriptLanguage::JavaScript.extension(), ".js");
        assert_eq!(ScriptLanguage::TypeScript.extension(), ".ts");
        assert_eq!(ScriptLanguage::Go.extension(), ".go");
    }

    #[test]
    fn test_is_shell() {
        assert!(ScriptLanguage::Shell.is_shell());
        assert!(!ScriptLanguage::Python.is_shell());
        assert!(!ScriptLanguage::JavaScript.is_shell());
        assert!(!ScriptLanguage::TypeScript.is_shell());
        assert!(!ScriptLanguage::Go.is_shell());
    }

    #[test]
    fn test_display() {
        assert_eq!(ScriptLanguage::Shell.to_string(), "shell");
        assert_eq!(ScriptLanguage::Python.to_string(), "python");
        assert_eq!(ScriptLanguage::JavaScript.to_string(), "javascript");
        assert_eq!(ScriptLanguage::TypeScript.to_string(), "typescript");
        assert_eq!(ScriptLanguage::Go.to_string(), "go");
    }

    #[test]
    fn test_valid_language_names_constant() {
        assert_eq!(VALID_LANGUAGE_NAMES.len(), 7);
        assert!(VALID_LANGUAGE_NAMES.contains(&"shell"));
        assert!(VALID_LANGUAGE_NAMES.contains(&"python"));
        assert!(VALID_LANGUAGE_NAMES.contains(&"javascript"));
        assert!(VALID_LANGUAGE_NAMES.contains(&"typescript"));
        assert!(VALID_LANGUAGE_NAMES.contains(&"go"));
        assert!(VALID_LANGUAGE_NAMES.contains(&"js"));
        assert!(VALID_LANGUAGE_NAMES.contains(&"ts"));
    }
}
