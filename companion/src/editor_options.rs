//! The pinned ty server's initialization/workspace settings precedence.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// ty rejects the complete ClientOptions value when its LSP fields have wrong
/// types. Validation of a well-shaped inline ty configuration is a separate
/// layer: failure there must not discard the selected Python environment.
pub fn validate_workspace(value: &Value) -> Result<()> {
    fn field<'a>(
        value: &'a Value,
        key: &str,
        accepts: fn(&Value) -> bool,
    ) -> Result<Option<&'a Value>> {
        let value = value.get(key).filter(|value| !value.is_null());
        ensure!(
            value.is_none_or(accepts),
            "invalid workspace option `{key}`"
        );
        Ok(value)
    }
    if value.is_null() {
        return Ok(());
    }
    ensure!(value.is_object(), "workspace options must be an object");
    field(value, "configuration", Value::is_object)?;
    field(value, "configurationFile", Value::is_string)?;
    for key in ["disableLanguageServices", "showSyntaxErrors"] {
        field(value, key, Value::is_boolean)?;
    }
    field(value, "diagnosticMode", |value| {
        matches!(value.as_str(), Some("off" | "openFilesOnly" | "workspace"))
    })?;
    for (key, fields) in [
        ("inlayHints", ["variableTypes", "callArgumentNames"]),
        ("completions", ["autoImport", "completeFunctionParentheses"]),
    ] {
        if let Some(options) = field(value, key, Value::is_object)? {
            for key in fields {
                field(options, key, Value::is_boolean)?;
            }
        }
    }
    if let Some(options) = field(value, "experimental", Value::is_object)? {
        field(options, "useUv", |value| {
            matches!(value.as_str(), Some("off" | "scripts" | "on"))
        })?;
    }
    if let Some(extension) = field(value, "pythonExtension", Value::is_object)?
        && let Some(environment) = field(extension, "activeEnvironment", Value::is_object)?
    {
        let executable = field(environment, "executable", Value::is_object)?
            .context("activeEnvironment requires executable")?;
        field(executable, "sysPrefix", Value::is_string)?
            .context("executable requires sysPrefix")?;
        field(executable, "uri", Value::is_string)?;
        if let Some(version) = field(environment, "version", Value::is_object)? {
            for key in ["major", "minor"] {
                field(version, key, Value::is_i64)?
                    .with_context(|| format!("environment version requires {key}"))?;
            }
            field(version, "patch", Value::is_i64)?;
            field(version, "sysVersion", Value::is_string)?;
        }
        if let Some(environment) = field(environment, "environment", Value::is_object)? {
            for key in ["folderUri", "type", "name"] {
                field(environment, key, Value::is_string)?;
            }
        }
    }
    Ok(())
}

/// Native InitializationOptions defaults the entire malformed object, while
/// preserving trust. Invalid trust itself is an initialization error.
pub fn initialization(value: &Value) -> Result<Value> {
    let trust = value.get("untrustedWorkspace");
    ensure!(
        trust.is_none_or(|value| value.is_null() || value.is_boolean()),
        "Invalid `untrustedWorkspace` setting"
    );
    if value.is_null() {
        return Ok(json!({}));
    }
    let valid_log_level = value.get("logLevel").is_none_or(|value| {
        value.is_null()
            || matches!(
                value.as_str(),
                Some("error" | "warn" | "info" | "debug" | "trace")
            )
    });
    let valid_log_file = value
        .get("logFile")
        .is_none_or(|value| value.is_null() || value.is_string());
    if validate_workspace(value).is_ok() && valid_log_level && valid_log_file {
        return Ok(value.clone());
    }
    Ok(trust.map_or_else(|| json!({}), |trust| json!({"untrustedWorkspace":trust})))
}

/// Initialization fields take precedence, except for ty's diagnostic-mode
/// special case. Configuration maps are atomic, not recursively combined.
pub fn effective(initialization: &Value, workspace: &Value) -> Value {
    let initialization = self::initialization(initialization).unwrap_or_else(|_| json!({}));
    let mut merged = workspace.as_object().cloned().unwrap_or_default();
    if let Some(initialization) = initialization.as_object() {
        for (key, value) in initialization {
            if value.is_null() {
                continue;
            }
            if key == "diagnosticMode"
                && merged
                    .get(key)
                    .is_some_and(|mode| mode == "off" || mode == "workspace")
            {
                continue;
            }
            if matches!(key.as_str(), "inlayHints" | "completions" | "experimental")
                && let Some(values) = value.as_object()
            {
                let mut options = merged
                    .get(key)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                options.extend(
                    values
                        .iter()
                        .filter(|(_, v)| !v.is_null())
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
                merged.insert(key.clone(), Value::Object(options));
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}

/// The native checker is an internal oracle. Typing configuration has already
/// been relocated by Session; presentation-only restrictions are applied by
/// the outer LSP, so they cannot disable probes or hide syntax diagnostics.
pub fn backend(settings: &Value) -> Value {
    let mut settings = settings.as_object().cloned().unwrap_or_default();
    settings.remove("configuration");
    settings.remove("configurationFile");
    settings.insert("diagnosticMode".into(), json!("openFilesOnly"));
    settings.insert("showSyntaxErrors".into(), json!(true));
    settings.insert("disableLanguageServices".into(), json!(false));
    Value::Object(settings)
}

pub fn language_services_response(method: &str) -> Option<Value> {
    match method {
        "textDocument/hover"
        | "textDocument/completion"
        | "textDocument/signatureHelp"
        | "textDocument/definition"
        | "textDocument/declaration"
        | "textDocument/typeDefinition"
        | "textDocument/implementation"
        | "textDocument/references"
        | "textDocument/documentHighlight"
        | "textDocument/documentSymbol"
        | "textDocument/foldingRange"
        | "textDocument/selectionRange"
        | "textDocument/inlayHint"
        | "textDocument/prepareRename"
        | "textDocument/rename"
        | "textDocument/prepareCallHierarchy"
        | "textDocument/prepareTypeHierarchy"
        | "textDocument/semanticTokens/full"
        | "textDocument/semanticTokens/range" => Some(Value::Null),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_native_initialization_precedence() {
        let init = json!({"configuration":{"rules":{"invalid-assignment":"warn"}}, "inlayHints":{"variableTypes":false}, "diagnosticMode":"workspace", "showSyntaxErrors":false});
        let workspace = json!({"configuration":{"environment":{"python-version":"3.10"}}, "inlayHints":{"variableTypes":true,"callArgumentNames":false}, "diagnosticMode":"openFilesOnly", "showSyntaxErrors":true,"pythonExtension":{"activeEnvironment":null}});
        let merged = effective(&init, &workspace);
        assert_eq!(merged["configuration"], init["configuration"]);
        assert_eq!(
            merged["inlayHints"],
            json!({"variableTypes":false,"callArgumentNames":false})
        );
        assert_eq!(merged["diagnosticMode"], "workspace");
        assert_eq!(merged["showSyntaxErrors"], false);
        assert_eq!(merged["pythonExtension"], workspace["pythonExtension"]);
        assert_eq!(
            effective(&init, &json!({"diagnosticMode":"off"}))["diagnosticMode"],
            "off"
        );
        assert_eq!(effective(&Value::Null, &Value::Null), json!({}));
    }

    #[test]
    fn malformed_workspace_fields_reject_the_whole_client_options() {
        for value in [
            json!({"configuration":[]}),
            json!({"inlayHints":{"variableTypes":"no"}}),
            json!({"diagnosticMode":"unknown"}),
            json!({"pythonExtension":{"activeEnvironment":{"executable":{}}}}),
        ] {
            assert!(validate_workspace(&value).is_err(), "{value}");
        }
        assert!(validate_workspace(&json!({"configuration":{"unknown-option":42}, "pythonExtension":{"activeEnvironment":{"executable":{"sysPrefix":"/environment"}, "version":{"major":3,"minor":12}}}})).is_ok());
        assert!(validate_workspace(&Value::Null).is_ok());
    }

    #[test]
    fn malformed_initialization_discards_options_without_granting_trust() {
        let invalid = json!({"untrustedWorkspace":true, "inlayHints":{"variableTypes":"bad"}, "configuration":{"rules":{"invalid-assignment":"ignore"}}});
        assert_eq!(
            initialization(&invalid).unwrap(),
            json!({"untrustedWorkspace":true})
        );
        assert!(effective(&invalid, &Value::Null)["configuration"].is_null());
        assert!(initialization(&json!({"untrustedWorkspace":"false"})).is_err());
        assert_eq!(
            initialization(&json!({"untrustedWorkspace":null})).unwrap()["untrustedWorkspace"],
            Value::Null
        );
    }
}
