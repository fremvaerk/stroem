use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use stroem_common::language::ScriptLanguage;
use uuid::Uuid;

/// Returns preference-ordered list of (binary, pre_args) for a language.
/// The first binary found on `$PATH` will be used.
pub fn toolchain_preferences(lang: ScriptLanguage) -> Vec<(&'static str, Vec<&'static str>)> {
    match lang {
        ScriptLanguage::Shell => vec![("bash", vec![]), ("sh", vec![])],
        ScriptLanguage::Python => {
            vec![("uv", vec!["run"]), ("python3", vec![]), ("python", vec![])]
        }
        ScriptLanguage::JavaScript => vec![("bun", vec!["run"]), ("node", vec![])],
        ScriptLanguage::TypeScript => vec![("bun", vec!["run"]), ("deno", vec!["run"])],
        ScriptLanguage::Go => vec![("go", vec!["run"])],
    }
}

/// Resolve which interpreter binary to use for a language.
///
/// If `interpreter_override` is set, use it directly (skip preference resolution).
/// Otherwise, probe each preference via `which::which` and return the first found.
///
/// Returns `(binary, pre_args)`.
pub fn resolve_toolchain(
    lang: ScriptLanguage,
    interpreter_override: Option<&str>,
) -> Result<(String, Vec<String>)> {
    if let Some(interp) = interpreter_override {
        return Ok((interp.to_string(), vec![]));
    }

    let prefs = toolchain_preferences(lang);
    for (binary, pre_args) in &prefs {
        if which::which(binary).is_ok() {
            return Ok((
                binary.to_string(),
                pre_args.iter().map(|s| s.to_string()).collect(),
            ));
        }
    }

    let tried: Vec<&str> = prefs.iter().map(|(b, _)| *b).collect();
    bail!(
        "No interpreter found for language '{}'. Tried: {}",
        lang.as_str(),
        tried.join(", ")
    )
}

/// Shell-escape a single token by wrapping it in single quotes.
///
/// Internal single quotes are replaced with `'\''` (end quote, escaped quote, resume quote),
/// which is the POSIX-portable way to include a literal `'` inside a single-quoted string.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the dependency arguments for a given language and interpreter.
///
/// Returns args to prepend to the run command. For Python+uv, returns
/// `["--with", "dep1", "--with", "dep2"]` which go between `run` and the script path.
/// For others, returns a shell prefix command (install && run).
pub fn build_dep_install_prefix(
    lang: ScriptLanguage,
    binary: &str,
    deps: &[String],
) -> Option<String> {
    if deps.is_empty() {
        return None;
    }

    match lang {
        ScriptLanguage::Python if binary == "uv" => {
            // uv handles deps inline via --with, handled in build_script_command
            None
        }
        ScriptLanguage::Python => {
            let dep_list = deps
                .iter()
                .map(|d| shell_escape(d))
                .collect::<Vec<_>>()
                .join(" ");
            Some(format!("{binary} -m pip install -q {dep_list}"))
        }
        ScriptLanguage::JavaScript | ScriptLanguage::TypeScript if binary == "bun" => {
            let dep_list = deps
                .iter()
                .map(|d| shell_escape(d))
                .collect::<Vec<_>>()
                .join(" ");
            Some(format!("bun install {dep_list}"))
        }
        ScriptLanguage::JavaScript | ScriptLanguage::TypeScript if binary == "node" => {
            let dep_list = deps
                .iter()
                .map(|d| shell_escape(d))
                .collect::<Vec<_>>()
                .join(" ");
            Some(format!("npm install --no-save {dep_list}"))
        }
        _ => None,
    }
}

/// Build uv --with args for inline dependency installation.
pub fn build_uv_with_args(deps: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for dep in deps {
        args.push("--with".to_string());
        args.push(dep.clone());
    }
    args
}

/// Write a temporary script file in the given directory.
/// Returns the path to the created file.
pub fn write_temp_script(workdir: &Path, content: &str, lang: ScriptLanguage) -> Result<PathBuf> {
    let ext = lang.extension();
    let filename = format!("_stroem_script_{}{}", Uuid::new_v4(), ext);
    let path = workdir.join(&filename);
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write temp script: {}", path.display()))?;

    // Make executable on Unix (owner-only: no group/other access)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&path, perms)
            .with_context(|| format!("Failed to set permissions on {}", path.display()))?;
    }

    Ok(path)
}

/// Best-effort cleanup of a temporary script file.
pub fn cleanup_temp_script(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Build the full command (binary + args) to execute a script file.
///
/// Returns `(program, args)` where args includes any pre-args + dep args + script path + script_args.
pub fn build_script_command(
    lang: ScriptLanguage,
    script_path: &Path,
    deps: &[String],
    interpreter_override: Option<&str>,
    script_args: &[String],
) -> Result<(String, Vec<String>)> {
    let (binary, pre_args) = resolve_toolchain(lang, interpreter_override)?;

    let mut args = pre_args;

    // For uv, inject --with args between "run" and the script path
    if binary == "uv" && !deps.is_empty() {
        args.extend(build_uv_with_args(deps));
    }

    args.push(script_path.to_string_lossy().to_string());

    // Append user-provided script arguments
    args.extend(script_args.iter().cloned());

    Ok((binary, args))
}

/// Build a `sh -c` command string for running a script FILE inside a container.
///
/// Unlike [`build_container_script_cmd`], which writes inline script content via a
/// heredoc, this function assumes the file already exists at `file_path` on the
/// container filesystem (e.g. `/workspace/actions/my_script.py`) and constructs a
/// shell command to invoke it with the appropriate interpreter.
///
/// For plain shell scripts with no dependencies and no interpreter override this
/// reduces to `sh -c /path/to/script.sh`. For other languages the function generates
/// the same dependency-install and `command -v` detection chain used by
/// [`build_container_script_cmd`], substituting the caller-supplied file path in
/// place of the heredoc-written temp file.
///
/// # Examples
///
/// ```
/// use stroem_runner::script_exec::build_container_file_cmd;
/// use stroem_common::language::ScriptLanguage;
///
/// // Shell: simple passthrough
/// let cmd = build_container_file_cmd("/workspace/run.sh", ScriptLanguage::Shell, &[], None, &[]);
/// assert_eq!(cmd, vec!["sh", "-c", "/workspace/run.sh"]);
///
/// // Python: generates interpreter-detection chain pointing at the file (path is shell-escaped)
/// let cmd = build_container_file_cmd("/workspace/hello.py", ScriptLanguage::Python, &[], None, &[]);
/// assert_eq!(cmd[0], "sh");
/// assert!(cmd[2].contains("command -v uv"));
/// assert!(cmd[2].contains("'/workspace/hello.py'"));
/// ```
pub fn build_container_file_cmd(
    file_path: &str,
    lang: ScriptLanguage,
    deps: &[String],
    interpreter_override: Option<&str>,
    script_args: &[String],
) -> Vec<String> {
    let args_suffix = if script_args.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            script_args
                .iter()
                .map(|a| shell_escape(a))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    if lang.is_shell() && deps.is_empty() && interpreter_override.is_none() {
        if script_args.is_empty() {
            return vec!["sh".to_string(), "-c".to_string(), file_path.to_string()];
        } else {
            // sh -c "file_path" sh arg1 arg2 — POSIX positional args ($1, $2, ...)
            let mut result = vec![
                "sh".to_string(),
                "-c".to_string(),
                file_path.to_string(),
                "sh".to_string(),
            ];
            result.extend(script_args.iter().cloned());
            return result;
        }
    }

    // Shell-escape the file path to prevent injection when interpolated into sh -c strings.
    // The passthrough case (shell + no deps + no interpreter + no args) returns exec-form
    // vectors where each element is a separate OS argument, so escaping is not needed there.
    let escaped_path = shell_escape(file_path);

    let mut script = String::new();

    // Build the interpreter invocation for the file path.
    // SECURITY: `interpreter_override` is validated at parse time to contain only
    // [a-zA-Z0-9._\-/+] characters (see validation.rs), so it is safe to interpolate
    // directly into shell command strings without escaping.
    if let Some(interp) = interpreter_override {
        if interp == "uv" && !deps.is_empty() {
            let with_args = deps
                .iter()
                .map(|d| format!("--with {}", shell_escape(d)))
                .collect::<Vec<_>>()
                .join(" ");
            script.push_str(&format!(
                "{interp} run {with_args} {escaped_path}{args_suffix}"
            ));
        } else {
            // Install deps first (e.g. python3 -m pip install ...), then run the file.
            if let Some(install_cmd) = build_dep_install_prefix(lang, interp, deps) {
                script.push_str(&format!("{install_cmd}\n"));
            }
            let prefs = toolchain_preferences(lang);
            let pre_args = prefs
                .iter()
                .find(|(b, _)| *b == interp)
                .map(|(_, a)| a.clone())
                .unwrap_or_default();
            let pre = pre_args.join(" ");
            if pre.is_empty() {
                script.push_str(&format!("{interp} {escaped_path}{args_suffix}"));
            } else {
                script.push_str(&format!("{interp} {pre} {escaped_path}{args_suffix}"));
            }
        }
    } else {
        // Try each preference via `command -v`. Dep installation is embedded per-branch
        // so the binary that installs deps matches the one that runs the file.
        let prefs = toolchain_preferences(lang);
        let mut first = true;
        for (binary, pre_args) in &prefs {
            let pre = pre_args.join(" ");

            let mut branch = String::new();
            if binary == &"uv" && !deps.is_empty() {
                let with_args = deps
                    .iter()
                    .map(|d| format!("--with {}", shell_escape(d)))
                    .collect::<Vec<_>>()
                    .join(" ");
                branch.push_str(&format!(
                    "{binary} run {with_args} {escaped_path}{args_suffix}"
                ));
            } else {
                if let Some(install_cmd) = build_dep_install_prefix(lang, binary, deps) {
                    branch.push_str(&format!("{install_cmd}\n  "));
                }
                if pre.is_empty() {
                    branch.push_str(&format!("{binary} {escaped_path}{args_suffix}"));
                } else {
                    branch.push_str(&format!("{binary} {pre} {escaped_path}{args_suffix}"));
                }
            }

            if first {
                script.push_str(&format!(
                    "if command -v {binary} >/dev/null 2>&1; then\n  {branch}\n"
                ));
                first = false;
            } else {
                script.push_str(&format!(
                    "elif command -v {binary} >/dev/null 2>&1; then\n  {branch}\n"
                ));
            }
        }
        if !first {
            let tried: Vec<&str> = prefs.iter().map(|(b, _)| *b).collect();
            script.push_str(&format!(
                "else\n  echo \"Error: no interpreter found for '{}'. Tried: {}\" >&2\n  exit 127\nfi",
                lang.as_str(),
                tried.join(", ")
            ));
        }
    }

    vec!["sh".to_string(), "-c".to_string(), script]
}

/// Build a `sh -c` command string for running a script inside a container.
///
/// Containers have the workspace mounted read-only, so we write the script
/// via a quoted heredoc, then execute it with the appropriate interpreter.
/// Toolchain resolution uses `command -v` at runtime since we don't know
/// what's available in the container image.
///
/// Security properties:
/// - The heredoc delimiter is UUID-based to prevent content-injection attacks.
/// - The script path in `/tmp` includes a UUID to prevent collisions between
///   concurrent container executions.
/// - Dependency names are shell-escaped before being interpolated into commands.
pub fn build_container_script_cmd(
    content: &str,
    lang: ScriptLanguage,
    deps: &[String],
    interpreter_override: Option<&str>,
    script_args: &[String],
) -> Vec<String> {
    if lang.is_shell() && deps.is_empty() && interpreter_override.is_none() {
        if script_args.is_empty() {
            // Shell with no special config — use the original sh -c path
            return vec!["sh".to_string(), "-c".to_string(), content.to_string()];
        } else {
            // sh -c "code" sh arg1 arg2 — POSIX positional args ($1, $2, ...)
            let mut result = vec![
                "sh".to_string(),
                "-c".to_string(),
                content.to_string(),
                "sh".to_string(),
            ];
            result.extend(script_args.iter().cloned());
            return result;
        }
    }

    let args_suffix = if script_args.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            script_args
                .iter()
                .map(|a| shell_escape(a))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    let ext = lang.extension();
    // UUID in path prevents collisions between concurrent container executions (Fix I4).
    let script_file = format!("/tmp/_stroem_script_{}{}", Uuid::new_v4(), ext);
    // UUID in delimiter prevents script content from escaping the heredoc (Fix I5).
    let delimiter = format!("STROEM_EOF_{}", Uuid::new_v4().simple());

    // Build the heredoc that writes the script
    let mut script = format!(
        "cat > {script_file} << '{delimiter}'\n{content}\n{delimiter}\nchmod +x {script_file}\n"
    );

    // Build the interpreter invocation.
    // For the auto-detect path (no override), we embed dep installation inside the
    // `command -v` chain so the same binary that runs the script also installs deps
    // (Fix I9). For the override path, the caller has already chosen a binary.
    if let Some(interp) = interpreter_override {
        // Direct override — install deps (if any) then run.
        if interp == "uv" && !deps.is_empty() {
            let with_args = deps
                .iter()
                .map(|d| format!("--with {}", shell_escape(d)))
                .collect::<Vec<_>>()
                .join(" ");
            script.push_str(&format!(
                "{interp} run {with_args} {script_file}{args_suffix}"
            ));
        } else {
            if let Some(install_cmd) = build_dep_install_prefix(lang, interp, deps) {
                script.push_str(&format!("{install_cmd}\n"));
            }
            let prefs = toolchain_preferences(lang);
            let pre_args = prefs
                .iter()
                .find(|(b, _)| *b == interp)
                .map(|(_, a)| a.clone())
                .unwrap_or_default();
            let pre = pre_args.join(" ");
            if pre.is_empty() {
                script.push_str(&format!("{interp} {script_file}{args_suffix}"));
            } else {
                script.push_str(&format!("{interp} {pre} {script_file}{args_suffix}"));
            }
        }
    } else {
        // Try each preference via `command -v`. Dep installation is embedded inside
        // each branch so the binary that installs matches the binary that runs (Fix I9).
        let prefs = toolchain_preferences(lang);
        let mut first = true;
        for (binary, pre_args) in &prefs {
            let pre = pre_args.join(" ");

            // Build the full branch body: optional install + run.
            let mut branch = String::new();
            if binary == &"uv" && !deps.is_empty() {
                // uv handles deps inline via --with flags.
                let with_args = deps
                    .iter()
                    .map(|d| format!("--with {}", shell_escape(d)))
                    .collect::<Vec<_>>()
                    .join(" ");
                branch.push_str(&format!(
                    "{binary} run {with_args} {script_file}{args_suffix}"
                ));
            } else {
                if let Some(install_cmd) = build_dep_install_prefix(lang, binary, deps) {
                    branch.push_str(&format!("{install_cmd}\n  "));
                }
                if pre.is_empty() {
                    branch.push_str(&format!("{binary} {script_file}{args_suffix}"));
                } else {
                    branch.push_str(&format!("{binary} {pre} {script_file}{args_suffix}"));
                }
            }

            if first {
                script.push_str(&format!(
                    "if command -v {binary} >/dev/null 2>&1; then\n  {branch}\n"
                ));
                first = false;
            } else {
                script.push_str(&format!(
                    "elif command -v {binary} >/dev/null 2>&1; then\n  {branch}\n"
                ));
            }
        }
        if !first {
            let tried: Vec<&str> = prefs.iter().map(|(b, _)| *b).collect();
            script.push_str(&format!(
                "else\n  echo \"Error: no interpreter found for '{}'. Tried: {}\" >&2\n  exit 127\nfi",
                lang.as_str(),
                tried.join(", ")
            ));
        }
    }

    vec!["sh".to_string(), "-c".to_string(), script]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toolchain_preferences_shell() {
        let prefs = toolchain_preferences(ScriptLanguage::Shell);
        assert_eq!(prefs[0].0, "bash");
        assert_eq!(prefs[1].0, "sh");
    }

    #[test]
    fn test_toolchain_preferences_python() {
        let prefs = toolchain_preferences(ScriptLanguage::Python);
        assert_eq!(prefs[0].0, "uv");
        assert_eq!(prefs[1].0, "python3");
        assert_eq!(prefs[2].0, "python");
    }

    #[test]
    fn test_toolchain_preferences_javascript() {
        let prefs = toolchain_preferences(ScriptLanguage::JavaScript);
        assert_eq!(prefs[0].0, "bun");
        assert_eq!(prefs[1].0, "node");
    }

    #[test]
    fn test_toolchain_preferences_typescript() {
        let prefs = toolchain_preferences(ScriptLanguage::TypeScript);
        assert_eq!(prefs[0].0, "bun");
        assert_eq!(prefs[1].0, "deno");
    }

    #[test]
    fn test_toolchain_preferences_go() {
        let prefs = toolchain_preferences(ScriptLanguage::Go);
        assert_eq!(prefs[0].0, "go");
    }

    #[test]
    fn test_resolve_toolchain_with_override() {
        let (binary, pre_args) =
            resolve_toolchain(ScriptLanguage::Python, Some("python3.12")).unwrap();
        assert_eq!(binary, "python3.12");
        assert!(pre_args.is_empty());
    }

    #[test]
    fn test_resolve_toolchain_shell_finds_sh() {
        // sh should always be available
        let result = resolve_toolchain(ScriptLanguage::Shell, None);
        assert!(result.is_ok());
        let (binary, _) = result.unwrap();
        assert!(binary == "bash" || binary == "sh");
    }

    #[test]
    fn test_build_uv_with_args() {
        let args = build_uv_with_args(&["requests".to_string(), "click".to_string()]);
        assert_eq!(args, vec!["--with", "requests", "--with", "click"]);
    }

    #[test]
    fn test_build_uv_with_args_empty() {
        let args = build_uv_with_args(&[]);
        assert!(args.is_empty());
    }

    #[test]
    fn test_build_dep_install_prefix_python_pip() {
        let result =
            build_dep_install_prefix(ScriptLanguage::Python, "python3", &["requests".to_string()]);
        assert_eq!(
            result,
            Some("python3 -m pip install -q 'requests'".to_string())
        );
    }

    #[test]
    fn test_build_dep_install_prefix_python_uv() {
        // uv handles deps inline, so no prefix
        let result =
            build_dep_install_prefix(ScriptLanguage::Python, "uv", &["requests".to_string()]);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_dep_install_prefix_js_bun() {
        let result =
            build_dep_install_prefix(ScriptLanguage::JavaScript, "bun", &["lodash".to_string()]);
        assert_eq!(result, Some("bun install 'lodash'".to_string()));
    }

    #[test]
    fn test_build_dep_install_prefix_js_node() {
        let result =
            build_dep_install_prefix(ScriptLanguage::JavaScript, "node", &["lodash".to_string()]);
        assert_eq!(result, Some("npm install --no-save 'lodash'".to_string()));
    }

    #[test]
    fn test_build_dep_install_prefix_empty() {
        let result = build_dep_install_prefix(ScriptLanguage::Python, "python3", &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_write_and_cleanup_temp_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_script(dir.path(), "print('hello')", ScriptLanguage::Python).unwrap();
        assert!(path.exists());
        assert!(path.extension().unwrap() == "py");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "print('hello')");

        cleanup_temp_script(&path);
        assert!(!path.exists());
    }

    #[test]
    fn test_write_temp_script_extensions() {
        let dir = tempfile::tempdir().unwrap();

        let sh = write_temp_script(dir.path(), "echo hi", ScriptLanguage::Shell).unwrap();
        assert_eq!(sh.extension().unwrap(), "sh");

        let js =
            write_temp_script(dir.path(), "console.log('hi')", ScriptLanguage::JavaScript).unwrap();
        assert_eq!(js.extension().unwrap(), "js");

        let ts =
            write_temp_script(dir.path(), "console.log('hi')", ScriptLanguage::TypeScript).unwrap();
        assert_eq!(ts.extension().unwrap(), "ts");

        let go = write_temp_script(dir.path(), "package main", ScriptLanguage::Go).unwrap();
        assert_eq!(go.extension().unwrap(), "go");
    }

    #[test]
    fn test_build_container_script_cmd_shell_passthrough() {
        let cmd = build_container_script_cmd("echo hello", ScriptLanguage::Shell, &[], None, &[]);
        assert_eq!(cmd, vec!["sh", "-c", "echo hello"]);
    }

    #[test]
    fn test_build_container_script_cmd_python() {
        let cmd =
            build_container_script_cmd("print('hello')", ScriptLanguage::Python, &[], None, &[]);
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Delimiter is UUID-based: verify the pattern rather than the exact value.
        assert!(cmd[2].contains("STROEM_EOF_"));
        assert!(cmd[2].contains("print('hello')"));
        assert!(cmd[2].contains("command -v uv"));
        assert!(cmd[2].contains("command -v python3"));
    }

    #[test]
    fn test_build_container_script_cmd_delimiter_is_unique() {
        // Each invocation must produce a different delimiter so concurrent containers
        // cannot collide or inject content across heredocs.
        let cmd1 = build_container_script_cmd("x=1", ScriptLanguage::Python, &[], None, &[]);
        let cmd2 = build_container_script_cmd("x=1", ScriptLanguage::Python, &[], None, &[]);
        let delim1 = cmd1[2].lines().find(|l| l.contains("STROEM_EOF_")).unwrap();
        let delim2 = cmd2[2].lines().find(|l| l.contains("STROEM_EOF_")).unwrap();
        assert_ne!(
            delim1, delim2,
            "delimiters must be unique across invocations"
        );
    }

    #[test]
    fn test_build_container_script_cmd_script_path_is_unique() {
        // Each invocation must produce a different /tmp script path.
        let cmd1 = build_container_script_cmd("x=1", ScriptLanguage::Python, &[], None, &[]);
        let cmd2 = build_container_script_cmd("x=1", ScriptLanguage::Python, &[], None, &[]);
        let path1 = cmd1[2]
            .lines()
            .find(|l| l.contains("/tmp/_stroem_script_"))
            .unwrap();
        let path2 = cmd2[2]
            .lines()
            .find(|l| l.contains("/tmp/_stroem_script_"))
            .unwrap();
        assert_ne!(
            path1, path2,
            "script paths must be unique across invocations"
        );
    }

    #[test]
    fn test_build_container_script_cmd_with_deps_shell_escaped() {
        // A dep name containing a single quote must be properly escaped.
        let tricky = "pkg'evil";
        let cmd = build_container_script_cmd(
            "import pkg",
            ScriptLanguage::Python,
            &[tricky.to_string()],
            None,
            &[],
        );
        assert!(
            cmd[2].contains("'pkg'\\''evil'"),
            "single quotes in dep names must be shell-escaped; got: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_script_cmd_with_interpreter_override() {
        let cmd = build_container_script_cmd(
            "print('hello')",
            ScriptLanguage::Python,
            &[],
            Some("python3.12"),
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // The path contains a UUID, so match on the stable prefix and extension.
        assert!(cmd[2].contains("python3.12 /tmp/_stroem_script_"));
        assert!(cmd[2].contains(".py"));
    }

    #[test]
    fn test_build_container_script_cmd_with_deps() {
        let cmd = build_container_script_cmd(
            "import requests",
            ScriptLanguage::Python,
            &["requests".to_string()],
            None,
            &[],
        );
        assert!(cmd[2].contains("--with 'requests'"));
    }

    #[test]
    fn test_build_container_script_cmd_javascript() {
        let cmd = build_container_script_cmd(
            "console.log('hi')",
            ScriptLanguage::JavaScript,
            &[],
            None,
            &[],
        );
        assert!(cmd[2].contains("command -v bun"));
        assert!(cmd[2].contains("command -v node"));
        // The path contains a UUID, so match on the stable prefix and extension.
        assert!(cmd[2].contains("/tmp/_stroem_script_"));
        assert!(cmd[2].contains(".js"));
    }

    #[test]
    fn test_build_container_script_cmd_go() {
        let cmd = build_container_script_cmd(
            "package main\nfunc main() {}",
            ScriptLanguage::Go,
            &[],
            None,
            &[],
        );
        // The path contains a UUID, so match on the stable prefix and extension.
        assert!(cmd[2].contains("/tmp/_stroem_script_"));
        assert!(cmd[2].contains(".go"));
        assert!(cmd[2].contains("go run"));
    }

    // --- build_container_file_cmd tests ---

    #[test]
    fn test_build_container_file_cmd_shell_passthrough() {
        let cmd =
            build_container_file_cmd("/workspace/run.sh", ScriptLanguage::Shell, &[], None, &[]);
        assert_eq!(cmd, vec!["sh", "-c", "/workspace/run.sh"]);
    }

    #[test]
    fn test_build_container_file_cmd_python_no_deps() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/hello.py",
            ScriptLanguage::Python,
            &[],
            None,
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must use command -v chain, NOT write a heredoc.
        assert!(!cmd[2].contains("STROEM_EOF"), "must not use heredoc");
        assert!(cmd[2].contains("command -v uv"));
        assert!(cmd[2].contains("command -v python3"));
        assert!(cmd[2].contains("'/workspace/actions/hello.py'"));
        // uv branch uses "uv run '/workspace/...'" (path is shell-escaped)
        assert!(cmd[2].contains("uv run '/workspace/actions/hello.py'"));
    }

    #[test]
    fn test_build_container_file_cmd_python_with_deps() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/fetch.py",
            ScriptLanguage::Python,
            &["requests".to_string()],
            None,
            &[],
        );
        assert!(!cmd[2].contains("STROEM_EOF"), "must not use heredoc");
        // uv branch: --with flag
        assert!(cmd[2].contains("--with 'requests'"));
        assert!(cmd[2].contains("'/workspace/actions/fetch.py'"));
    }

    #[test]
    fn test_build_container_file_cmd_python_interpreter_override() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/hello.py",
            ScriptLanguage::Python,
            &[],
            Some("python3.12"),
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert!(!cmd[2].contains("STROEM_EOF"), "must not use heredoc");
        assert!(cmd[2].contains("python3.12 '/workspace/actions/hello.py'"));
    }

    #[test]
    fn test_build_container_file_cmd_python_uv_override_with_deps() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/run.py",
            ScriptLanguage::Python,
            &["httpx".to_string()],
            Some("uv"),
            &[],
        );
        assert!(cmd[2].contains("uv run --with 'httpx' '/workspace/actions/run.py'"));
    }

    #[test]
    fn test_build_container_file_cmd_javascript() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/script.js",
            ScriptLanguage::JavaScript,
            &[],
            None,
            &[],
        );
        assert!(!cmd[2].contains("STROEM_EOF"), "must not use heredoc");
        assert!(cmd[2].contains("command -v bun"));
        assert!(cmd[2].contains("command -v node"));
        assert!(cmd[2].contains("'/workspace/actions/script.js'"));
    }

    #[test]
    fn test_build_container_file_cmd_typescript() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/task.ts",
            ScriptLanguage::TypeScript,
            &[],
            None,
            &[],
        );
        assert!(!cmd[2].contains("STROEM_EOF"), "must not use heredoc");
        assert!(cmd[2].contains("command -v bun"));
        assert!(cmd[2].contains("command -v deno"));
        assert!(cmd[2].contains("'/workspace/actions/task.ts'"));
    }

    #[test]
    fn test_build_container_file_cmd_go() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/main.go",
            ScriptLanguage::Go,
            &[],
            None,
            &[],
        );
        assert!(!cmd[2].contains("STROEM_EOF"), "must not use heredoc");
        assert!(cmd[2].contains("go run '/workspace/actions/main.go'"));
    }

    #[test]
    fn test_build_container_file_cmd_deps_shell_escaped() {
        let cmd = build_container_file_cmd(
            "/workspace/actions/fetch.py",
            ScriptLanguage::Python,
            &["pkg'evil".to_string()],
            None,
            &[],
        );
        assert!(
            cmd[2].contains("'pkg'\\''evil'"),
            "single quotes in dep names must be shell-escaped; got: {}",
            cmd[2]
        );
    }

    // --- build_dep_install_prefix additional tests ---

    #[test]
    fn test_build_dep_install_prefix_typescript_bun() {
        let result =
            build_dep_install_prefix(ScriptLanguage::TypeScript, "bun", &["ts-node".to_string()]);
        assert_eq!(result, Some("bun install 'ts-node'".to_string()));
    }

    #[test]
    fn test_build_dep_install_prefix_typescript_node() {
        let result =
            build_dep_install_prefix(ScriptLanguage::TypeScript, "node", &["ts-node".to_string()]);
        assert_eq!(result, Some("npm install --no-save 'ts-node'".to_string()));
    }

    #[test]
    fn test_build_dep_install_prefix_go() {
        // Go has no supported dep-install prefix — always returns None
        let result = build_dep_install_prefix(ScriptLanguage::Go, "go", &["some/pkg".to_string()]);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_dep_install_prefix_shell() {
        // Shell has no supported dep-install prefix — always returns None
        let result = build_dep_install_prefix(ScriptLanguage::Shell, "sh", &["dep".to_string()]);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_dep_install_prefix_multiple_deps() {
        let result = build_dep_install_prefix(
            ScriptLanguage::Python,
            "python3",
            &["requests".to_string(), "click".to_string()],
        );
        assert_eq!(
            result,
            Some("python3 -m pip install -q 'requests' 'click'".to_string())
        );
    }

    // --- build_container_script_cmd additional tests ---

    #[test]
    fn test_build_container_script_cmd_typescript() {
        // TypeScript without override uses the `command -v` detection chain.
        let cmd = build_container_script_cmd(
            "const x: number = 1; console.log(x);",
            ScriptLanguage::TypeScript,
            &[],
            None,
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must probe for bun and deno
        assert!(
            cmd[2].contains("command -v bun"),
            "expected bun probe; got: {}",
            cmd[2]
        );
        assert!(
            cmd[2].contains("command -v deno"),
            "expected deno probe; got: {}",
            cmd[2]
        );
        // Script file must use .ts extension
        assert!(
            cmd[2].contains(".ts"),
            "expected .ts extension in script path; got: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_script_cmd_shell_with_deps() {
        // Shell with deps must NOT use the passthrough path; it must write a heredoc.
        let cmd = build_container_script_cmd(
            "echo hello",
            ScriptLanguage::Shell,
            &["bash-dep".to_string()],
            None,
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must NOT be the simple "sh -c echo hello" passthrough
        assert_ne!(
            cmd[2], "echo hello",
            "shell with deps must not use passthrough"
        );
        // Must write the script via a heredoc
        assert!(
            cmd[2].contains("STROEM_EOF_"),
            "shell with deps must use heredoc; got: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_script_cmd_shell_with_interpreter() {
        // Shell with an interpreter override must NOT use the passthrough path.
        let cmd =
            build_container_script_cmd("echo hello", ScriptLanguage::Shell, &[], Some("bash"), &[]);
        assert_eq!(cmd[0], "sh");
        // Must NOT be the three-element passthrough
        assert!(
            cmd.len() == 3,
            "expected exactly [sh, -c, <script>]; got: {:?}",
            cmd
        );
        // Must write the script via a heredoc (because interpreter_override is set)
        assert!(
            cmd[2].contains("STROEM_EOF_"),
            "shell with interpreter override must use heredoc; got: {}",
            cmd[2]
        );
        assert!(
            cmd[2].contains("bash /tmp/_stroem_script_"),
            "must invoke bash on the script file; got: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_script_cmd_uv_interpreter_with_deps() {
        // Python + interpreter "uv" + deps → `uv run --with dep1 --with dep2 <script>`
        let cmd = build_container_script_cmd(
            "import httpx; print(httpx.get('http://example.com'))",
            ScriptLanguage::Python,
            &["httpx".to_string(), "certifi".to_string()],
            Some("uv"),
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // uv inline: uv run --with 'httpx' --with 'certifi' <script>
        assert!(
            cmd[2].contains("uv run --with 'httpx' --with 'certifi'"),
            "expected uv run --with args; got: {}",
            cmd[2]
        );
        // Must reference the temp script file, not pip install
        assert!(
            !cmd[2].contains("pip install"),
            "uv should not use pip; got: {}",
            cmd[2]
        );
    }

    #[test]
    fn test_build_container_script_cmd_unknown_interpreter() {
        // Python with an interpreter not in preferences should still work:
        // it writes a heredoc and invokes the given binary directly on the file.
        let cmd = build_container_script_cmd(
            "print('hello')",
            ScriptLanguage::Python,
            &[],
            Some("python3.12"),
            &[],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Must use the heredoc to write the file
        assert!(
            cmd[2].contains("STROEM_EOF_"),
            "must use heredoc; got: {}",
            cmd[2]
        );
        // Must invoke python3.12 on the temp script
        assert!(
            cmd[2].contains("python3.12 /tmp/_stroem_script_"),
            "must invoke python3.12 on the temp script; got: {}",
            cmd[2]
        );
        assert!(
            cmd[2].contains(".py"),
            "temp script must have .py extension; got: {}",
            cmd[2]
        );
    }

    // --- write_temp_script / cleanup_temp_script additional tests ---

    #[test]
    fn test_write_temp_script_nonexistent_dir() {
        let result = write_temp_script(
            std::path::Path::new("/nonexistent/directory/that/does/not/exist"),
            "print('hello')",
            ScriptLanguage::Python,
        );
        assert!(result.is_err(), "writing to a nonexistent dir must fail");
    }

    #[test]
    fn test_cleanup_temp_script_nonexistent_is_noop() {
        // Cleanup on a path that doesn't exist must not panic.
        let path = std::path::Path::new("/tmp/_stroem_does_not_exist_12345.py");
        // This must be a no-op (not panic).
        cleanup_temp_script(path);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_temp_script_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_script(dir.path(), "print('hello')", ScriptLanguage::Python).unwrap();
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o700,
            "script file must have 0o700 permissions"
        );
    }

    // ─── Script args tests ───────────────────────────────────────────────

    #[test]
    fn test_build_container_script_cmd_shell_with_args() {
        let args = vec!["--env".to_string(), "production".to_string()];
        let cmd = build_container_script_cmd("echo $1 $2", ScriptLanguage::Shell, &[], None, &args);
        // sh -c "echo $1 $2" sh --env production
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        assert_eq!(cmd[2], "echo $1 $2");
        assert_eq!(cmd[3], "sh"); // $0 placeholder
        assert_eq!(cmd[4], "--env");
        assert_eq!(cmd[5], "production");
    }

    #[test]
    fn test_build_container_script_cmd_shell_no_args_unchanged() {
        let cmd = build_container_script_cmd("echo hello", ScriptLanguage::Shell, &[], None, &[]);
        assert_eq!(cmd, vec!["sh", "-c", "echo hello"]);
    }

    #[test]
    fn test_build_container_script_cmd_python_with_args() {
        let args = vec!["--input".to_string(), "data.csv".to_string()];
        let cmd = build_container_script_cmd(
            "import sys; print(sys.argv)",
            ScriptLanguage::Python,
            &[],
            None,
            &args,
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // The command string should contain the args shell-escaped
        assert!(cmd[2].contains("--input"));
        assert!(cmd[2].contains("data.csv"));
    }

    #[test]
    fn test_build_container_file_cmd_shell_with_args() {
        let args = vec!["arg1".to_string(), "arg 2".to_string()];
        let cmd =
            build_container_file_cmd("/workspace/run.sh", ScriptLanguage::Shell, &[], None, &args);
        // sh -c "/workspace/run.sh" sh arg1 "arg 2"
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        assert_eq!(cmd[2], "/workspace/run.sh");
        assert_eq!(cmd[3], "sh"); // $0 placeholder
        assert_eq!(cmd[4], "arg1");
        assert_eq!(cmd[5], "arg 2");
    }

    #[test]
    fn test_build_container_file_cmd_shell_no_args_unchanged() {
        let cmd =
            build_container_file_cmd("/workspace/run.sh", ScriptLanguage::Shell, &[], None, &[]);
        assert_eq!(cmd, vec!["sh", "-c", "/workspace/run.sh"]);
    }

    #[test]
    fn test_build_container_file_cmd_python_with_args() {
        let args = vec!["--epochs".to_string(), "10".to_string()];
        let cmd = build_container_file_cmd(
            "/workspace/train.py",
            ScriptLanguage::Python,
            &[],
            None,
            &args,
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        // Args should be shell-escaped and appended after the file path
        assert!(cmd[2].contains("/workspace/train.py"));
        assert!(cmd[2].contains("--epochs"));
        assert!(cmd[2].contains("10"));
    }

    #[test]
    fn test_build_script_command_with_args() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("test.sh");
        std::fs::write(&script_path, "#!/bin/bash\necho $@").unwrap();

        let args = vec!["--verbose".to_string()];
        let (binary, result_args) = build_script_command(
            ScriptLanguage::Shell,
            &script_path,
            &[],
            Some("bash"),
            &args,
        )
        .unwrap();
        assert_eq!(binary, "bash");
        // Last arg should be "--verbose"
        assert_eq!(result_args.last().unwrap(), "--verbose");
        // Script path should be second-to-last
        assert!(result_args[result_args.len() - 2].contains("test.sh"));
    }

    #[test]
    fn test_build_container_file_cmd_args_shell_escaped() {
        let args = vec!["hello world".to_string(), "it's".to_string()];
        let cmd = build_container_file_cmd(
            "/workspace/run.py",
            ScriptLanguage::Python,
            &[],
            None,
            &args,
        );
        // Args with spaces and quotes should be shell-escaped in the command string
        let script = &cmd[2];
        assert!(
            script.contains("'hello world'") || script.contains("hello\\ world"),
            "expected shell-escaped 'hello world' in: {}",
            script
        );
    }

    #[test]
    fn test_build_container_script_cmd_typescript_with_args() {
        let args = vec!["--port".to_string(), "3000".to_string()];
        let cmd = build_container_script_cmd(
            "console.log('hi')",
            ScriptLanguage::TypeScript,
            &[],
            None,
            &args,
        );
        assert_eq!(cmd[0], "sh");
        assert!(cmd[2].contains("'--port'"));
        assert!(cmd[2].contains("'3000'"));
    }

    #[test]
    fn test_build_container_script_cmd_javascript_with_args() {
        let args = vec!["--verbose".to_string()];
        let cmd = build_container_script_cmd(
            "console.log('hi')",
            ScriptLanguage::JavaScript,
            &[],
            None,
            &args,
        );
        assert_eq!(cmd[0], "sh");
        assert!(cmd[2].contains("'--verbose'"));
    }

    #[test]
    fn test_build_container_script_cmd_go_with_args() {
        let args = vec!["--config".to_string(), "prod.yaml".to_string()];
        let cmd = build_container_script_cmd("package main", ScriptLanguage::Go, &[], None, &args);
        assert_eq!(cmd[0], "sh");
        assert!(cmd[2].contains("'--config'"));
        assert!(cmd[2].contains("'prod.yaml'"));
    }

    #[test]
    fn test_build_container_file_cmd_typescript_with_args() {
        let args = vec!["--port".to_string(), "8080".to_string()];
        let cmd = build_container_file_cmd(
            "/workspace/server.ts",
            ScriptLanguage::TypeScript,
            &[],
            None,
            &args,
        );
        assert_eq!(cmd[0], "sh");
        assert!(cmd[2].contains("'/workspace/server.ts'"));
        assert!(cmd[2].contains("'--port'"));
        assert!(cmd[2].contains("'8080'"));
    }

    #[test]
    fn test_build_container_file_cmd_javascript_with_args() {
        let args = vec!["run".to_string()];
        let cmd = build_container_file_cmd(
            "/workspace/index.js",
            ScriptLanguage::JavaScript,
            &[],
            None,
            &args,
        );
        assert_eq!(cmd[0], "sh");
        assert!(cmd[2].contains("'/workspace/index.js'"));
        assert!(cmd[2].contains("'run'"));
    }

    #[test]
    fn test_build_container_file_cmd_go_with_args() {
        let args = vec!["--flag".to_string()];
        let cmd =
            build_container_file_cmd("/workspace/main.go", ScriptLanguage::Go, &[], None, &args);
        assert_eq!(cmd[0], "sh");
        assert!(cmd[2].contains("'/workspace/main.go'"));
        assert!(cmd[2].contains("'--flag'"));
    }

    #[test]
    fn test_build_container_script_cmd_args_special_chars() {
        // Test shell metacharacters are properly escaped
        let args = vec![
            "$HOME".to_string(),
            "`whoami`".to_string(),
            "hello\nworld".to_string(),
            "a;b".to_string(),
        ];
        let cmd = build_container_script_cmd(
            "import sys; print(sys.argv)",
            ScriptLanguage::Python,
            &[],
            None,
            &args,
        );
        let script = &cmd[2];
        // All dangerous chars must be inside single quotes
        assert!(script.contains("'$HOME'"), "$ must be quoted: {}", script);
        assert!(
            script.contains("'`whoami`'"),
            "backticks must be quoted: {}",
            script
        );
        assert!(
            script.contains("'a;b'"),
            "semicolons must be quoted: {}",
            script
        );
    }

    #[test]
    fn test_build_container_script_cmd_args_single_quote_escaped() {
        let args = vec!["it's".to_string()];
        let cmd = build_container_script_cmd(
            "import sys; print(sys.argv)",
            ScriptLanguage::Python,
            &[],
            None,
            &args,
        );
        let script = &cmd[2];
        assert!(
            script.contains("'it'\\''s'"),
            "single quote must be escaped as '\\'' : {}",
            script
        );
    }

    #[test]
    fn test_build_container_file_cmd_args_single_quote_escaped() {
        let args = vec!["it's".to_string()];
        let cmd = build_container_file_cmd(
            "/workspace/run.py",
            ScriptLanguage::Python,
            &[],
            None,
            &args,
        );
        let script = &cmd[2];
        assert!(
            script.contains("'it'\\''s'"),
            "single quote must be escaped: {}",
            script
        );
    }

    #[test]
    fn test_build_container_file_cmd_python_with_deps_and_args() {
        let deps = vec!["requests".to_string()];
        let args = vec!["--input".to_string(), "data.csv".to_string()];
        let cmd = build_container_file_cmd(
            "/workspace/fetch.py",
            ScriptLanguage::Python,
            &deps,
            None,
            &args,
        );
        let script = &cmd[2];
        // uv branch: deps via --with, then file path, then args
        assert!(script.contains("--with 'requests'"), "deps: {}", script);
        assert!(script.contains("'/workspace/fetch.py'"), "path: {}", script);
        assert!(script.contains("'--input'"), "args: {}", script);
        assert!(script.contains("'data.csv'"), "args: {}", script);
    }

    #[test]
    fn test_build_container_script_cmd_python_with_deps_and_args() {
        let deps = vec!["pandas".to_string()];
        let args = vec!["--output".to_string(), "result.json".to_string()];
        let cmd =
            build_container_script_cmd("import pandas", ScriptLanguage::Python, &deps, None, &args);
        let script = &cmd[2];
        assert!(script.contains("--with 'pandas'"), "deps: {}", script);
        assert!(script.contains("'--output'"), "args: {}", script);
        assert!(script.contains("'result.json'"), "args: {}", script);
    }

    #[test]
    fn test_build_container_file_cmd_path_shell_escaped() {
        // Verify the security fix: file paths with spaces/metacharacters are escaped
        let cmd = build_container_file_cmd(
            "/workspace/my script.py",
            ScriptLanguage::Python,
            &[],
            None,
            &[],
        );
        let script = &cmd[2];
        assert!(
            script.contains("'/workspace/my script.py'"),
            "file path with spaces must be shell-escaped: {}",
            script
        );
    }
}
