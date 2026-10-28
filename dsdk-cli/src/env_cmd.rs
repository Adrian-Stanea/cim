// Copyright (c) 2026 Analog Devices, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use dsdk_cli::config::SdkConfig;
use dsdk_cli::workspace::{
    generate_git_dir_vars, generate_git_venv_vars, load_config_with_extends,
    require_workspace_config,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;

type Environment = HashMap<OsString, OsString>;

#[derive(Debug)]
enum Part {
    Literal(String),
    Reference(String),
}

/// Print fully resolved POSIX exports without modifying the workspace or shell.
pub(crate) fn handle_env_command() -> Result<(), Box<dyn std::error::Error>> {
    let (workspace, config_path) = require_workspace_config()?;
    let config = load_config_with_extends(&config_path)?;
    let environment = std::env::vars_os().collect();
    let definitions = collect_definitions(&config, &workspace, &environment)?;
    let values = resolve_definitions(&definitions)?;
    let output = render_exports(&values)?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn host_value(name: &str, environment: &Environment) -> Result<Option<String>, String> {
    environment
        .get(OsStr::new(name))
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("Environment variable '{name}' is not valid Unicode"))
        })
        .transpose()
}

fn require_host(name: &str, environment: &Environment) -> Result<String, String> {
    host_value(name, environment)?
        .ok_or_else(|| format!("Missing host environment variable '{name}'"))
}

fn collect_definitions(
    config: &SdkConfig,
    workspace: &Path,
    environment: &Environment,
) -> Result<BTreeMap<String, Vec<Part>>, String> {
    let user_vars = config.variables.as_ref();
    if user_vars.is_some_and(|vars| vars.contains_key("WORKSPACE")) {
        return Err("'WORKSPACE' is reserved for the detected workspace root".to_string());
    }

    let workspace = workspace
        .to_str()
        .ok_or("Workspace path is not valid Unicode")?;
    let mut definitions = BTreeMap::from([(
        "WORKSPACE".to_string(),
        vec![Part::Literal(workspace.to_string())],
    )]);
    let mut generated = generate_git_dir_vars(&config.gits, user_vars)?;
    generated.extend(generate_git_venv_vars(&config.gits, user_vars)?);
    for (name, value) in generated {
        let suffix = value
            .strip_prefix("${{ WORKSPACE }}")
            .ok_or_else(|| format!("Generated variable '{name}' has no workspace prefix"))?;
        definitions.insert(
            name,
            vec![
                Part::Reference("WORKSPACE".to_string()),
                Part::Literal(suffix.to_string()),
            ],
        );
    }
    if let Some(vars) = user_vars {
        // Sort before validation so both errors and successful output are deterministic.
        let sorted: BTreeMap<_, _> = vars.iter().collect();
        for (name, value) in sorted {
            if !valid_name(name) {
                return Err(format!("Invalid POSIX export name '{name}'"));
            }
            let parts = match host_value(name, environment)? {
                Some(value) => vec![Part::Literal(value)],
                None => parse_template(value, environment)
                    .map_err(|e| format!("Variable '{name}': {e}"))?,
            };
            definitions.insert(name.clone(), parts);
        }
    }
    Ok(definitions)
}

/// Parse definitions once: substituted host values must never become expressions.
fn parse_template(input: &str, environment: &Environment) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut rest = input;
    if rest == "~" || rest.starts_with("~/") {
        let home = match host_value("HOME", environment)? {
            Some(home) => home,
            None => require_host("USERPROFILE", environment)?,
        };
        literal.push_str(&home);
        rest = &rest[1..];
    }
    while !rest.is_empty() {
        if rest.starts_with('`') || rest.starts_with("$(") {
            return Err(
                "Shell/Make expressions are unsupported; use '${{ NAME }}' references".to_string(),
            );
        }
        if let Some(tail) = rest.strip_prefix("$$") {
            literal.push('$');
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("${{") {
            let end = tail.find("}}").ok_or("Unclosed manifest reference")?;
            let name = tail[..end].trim();
            if !valid_name(name) {
                return Err(format!("Invalid manifest reference name '{name}'"));
            }
            parts.push(Part::Literal(std::mem::take(&mut literal)));
            parts.push(Part::Reference(name.to_string()));
            rest = &tail[end + 2..];
        } else if let Some(tail) = rest.strip_prefix("${") {
            let end = tail
                .find('}')
                .ok_or("Unclosed host environment reference")?;
            let name = &tail[..end];
            if !valid_name(name) {
                return Err(format!("Unsupported host environment reference '{name}'"));
            }
            literal.push_str(&require_host(name, environment)?);
            rest = &tail[end + 1..];
        } else if let Some(tail) = rest.strip_prefix('$') {
            let len = tail
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            if len > 0 {
                let name = &tail[..len];
                if !valid_name(name) {
                    return Err(format!("Invalid host environment reference '{name}'"));
                }
                literal.push_str(&require_host(name, environment)?);
                rest = &tail[len..];
            } else {
                literal.push('$');
                rest = tail;
            }
        } else if let Some(tail) = rest.strip_prefix('%') {
            if let Some(end) = tail.find('%').filter(|&end| valid_name(&tail[..end])) {
                literal.push_str(&require_host(&tail[..end], environment)?);
                rest = &tail[end + 1..];
            } else {
                literal.push('%');
                rest = tail;
            }
        } else {
            let ch = rest.chars().next().ok_or("Invalid template")?;
            literal.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    parts.push(Part::Literal(literal));
    Ok(parts)
}

#[derive(Debug)]
struct Frame<'a> {
    name: &'a str,
    next_part: usize,
    value: String,
}

fn resolve_definitions(
    definitions: &BTreeMap<String, Vec<Part>>,
) -> Result<BTreeMap<String, String>, String> {
    let mut resolved = BTreeMap::<String, String>::new();
    for name in definitions.keys() {
        if resolved.contains_key(name) {
            continue;
        }
        // An explicit stack avoids overflowing the call stack on deep manifests.
        let mut stack = vec![Frame {
            name,
            next_part: 0,
            value: String::new(),
        }];
        while let Some(frame) = stack.last_mut() {
            let parts = definitions
                .get(frame.name)
                .ok_or_else(|| format!("Missing manifest variable '{}'", frame.name))?;
            match parts.get(frame.next_part) {
                Some(Part::Literal(value)) => {
                    frame.value.push_str(value);
                    frame.next_part += 1;
                }
                Some(Part::Reference(reference)) => {
                    if let Some(value) = resolved.get(reference) {
                        frame.value.push_str(value);
                        frame.next_part += 1;
                        continue;
                    }
                    let mut chain: Vec<_> = stack.iter().map(|f| f.name).collect();
                    if chain.contains(&reference.as_str()) {
                        chain.push(reference);
                        return Err(format!("Manifest variable cycle: {}", chain.join(" -> ")));
                    }
                    if !definitions.contains_key(reference) {
                        chain.push(reference);
                        return Err(format!(
                            "Missing manifest reference: {}",
                            chain.join(" -> ")
                        ));
                    }
                    stack.push(Frame {
                        name: reference,
                        next_part: 0,
                        value: String::new(),
                    });
                }
                None => {
                    let frame = stack.pop().ok_or("Invalid resolution state")?;
                    resolved.insert(frame.name.to_string(), frame.value);
                }
            }
        }
    }
    Ok(resolved)
}

fn render_exports(values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut output = String::new();
    for (name, value) in values {
        if !valid_name(name) {
            return Err(format!("Invalid POSIX export name '{name}'"));
        }
        if value.contains('\0') {
            return Err(format!("Variable '{name}' contains a NUL byte"));
        }
        writeln!(output, "export {name}='{}'", value.replace('\'', "'\\''"))
            .map_err(|e| e.to_string())?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsdk_cli::config::GitConfig;

    fn environment(values: &[(&str, &str)]) -> Environment {
        values
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    fn config(values: &[(&str, &str)]) -> SdkConfig {
        SdkConfig {
            variables: Some(
                values
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn resolve(config: &SdkConfig, env: &Environment) -> Result<BTreeMap<String, String>, String> {
        let definitions = collect_definitions(config, Path::new("/sdk"), env)?;
        resolve_definitions(&definitions)
    }

    #[test]
    fn resolves_nested_and_repeated_references() {
        let vars = resolve(
            &config(&[
                ("A", "${{ B }}/${{ B }}"),
                ("B", "${{ C }}"),
                ("C", "${{ WORKSPACE }}/bin"),
            ]),
            &Environment::new(),
        )
        .unwrap();
        assert_eq!(vars["A"], "/sdk/bin//sdk/bin");
    }

    #[test]
    fn resolves_host_syntax_and_literal_dollar_escape() {
        let vars = resolve(
            &config(&[("VALUE", "~/$HOST/${HOST}/%HOST%/$$COST/50%")]),
            &environment(&[("HOME", "/home/user"), ("HOST", "host")]),
        )
        .unwrap();
        assert_eq!(vars["VALUE"], "/home/user/host/host/host/$COST/50%");
        let vars = resolve(
            &config(&[("VALUE", "~/bin")]),
            &environment(&[("USERPROFILE", "/profile")]),
        )
        .unwrap();
        assert_eq!(vars["VALUE"], "/profile/bin");
    }

    #[test]
    fn host_values_and_overrides_are_literal_including_empty() {
        let payload = "${{ UNKNOWN }} $MISSING $(literal) `literal` '%MISSING%'";
        let vars = resolve(
            &config(&[
                ("VALUE", "$HOST"),
                ("OVERRIDE", "${{ MISSING }}"),
                ("EMPTY", "$MISSING"),
                ("CHAIN", "${{ OVERRIDE }}"),
            ]),
            &environment(&[("HOST", payload), ("OVERRIDE", payload), ("EMPTY", "")]),
        )
        .unwrap();
        assert_eq!(vars["VALUE"], payload);
        assert_eq!(vars["OVERRIDE"], payload);
        assert_eq!(vars["CHAIN"], payload);
        assert_eq!(vars["EMPTY"], "");
    }

    #[test]
    fn generated_paths_obey_precedence_and_python_deps() {
        let mut cfg = config(&[("OTHER_DIR", "${{ WORKSPACE }}/custom")]);
        cfg.gits = vec![
            GitConfig {
                name: "nested/repo".to_string(),
                python_deps: Some(vec!["requirements.txt".to_string()]),
                ..Default::default()
            },
            GitConfig {
                name: "other".to_string(),
                ..Default::default()
            },
        ];
        let vars = resolve(
            &cfg,
            &environment(&[
                ("WORKSPACE", "/stale"),
                ("REPO_DIR", "/stale/repo"),
                ("REPO_VENV", "/stale/venv"),
                ("OTHER_DIR", "/explicit"),
            ]),
        )
        .unwrap();
        assert_eq!(vars["WORKSPACE"], "/sdk");
        assert_eq!(vars["REPO_DIR"], "/sdk/nested/repo");
        assert_eq!(vars["REPO_VENV"], "/sdk/.cim/nested/repo/.venv");
        assert_eq!(vars["OTHER_DIR"], "/explicit");
        assert!(!vars.contains_key("OTHER_VENV"));
        let vars = resolve(&cfg, &Environment::new()).unwrap();
        assert_eq!(vars["OTHER_DIR"], "/sdk/custom");
    }

    #[test]
    fn generated_path_components_are_literal() {
        let mut cfg = config(&[]);
        cfg.gits = vec![GitConfig {
            name: "parent$MISSING/repo".to_string(),
            ..Default::default()
        }];
        assert_eq!(
            resolve(&cfg, &Environment::new()).unwrap()["REPO_DIR"],
            "/sdk/parent$MISSING/repo"
        );
    }

    #[test]
    fn rejects_missing_references_and_cycles_with_chain() {
        for (values, message) in [
            (vec![("A", "${{ B }}")], "A -> B"),
            (vec![("A", "${{ A }}")], "A -> A"),
            (
                vec![("A", "${{ B }}"), ("B", "${{ C }}"), ("C", "${{ A }}")],
                "A -> B -> C -> A",
            ),
        ] {
            let err = resolve(&config(&values), &Environment::new()).unwrap_err();
            assert!(err.contains(message), "{err}");
        }
    }

    #[test]
    fn rejects_unsupported_templates_without_disclosing_values() {
        for value in [
            "$MISSING",
            "${MISSING}",
            "%MISSING%",
            "${{ UNKNOWN }}",
            "${{ UNFINISHED",
            "${UNFINISHED",
            "${HOST:-default}",
            "${{ BAD-NAME }}",
            "$(WORKSPACE)",
            "`command`",
            "$1",
        ] {
            assert!(
                resolve(&config(&[("VALUE", value)]), &Environment::new()).is_err(),
                "{value}"
            );
        }
        let err = resolve(
            &config(&[("VALUE", "secret-prefix-$MISSING")]),
            &Environment::new(),
        )
        .unwrap_err();
        assert!(err.contains("VALUE"));
        assert!(!err.contains("secret-prefix"));
    }

    #[test]
    fn rejects_reserved_and_invalid_names_and_nuls() {
        for name in ["WORKSPACE", "", "1VAR", "BAD-NAME", "X; command"] {
            assert!(resolve(&config(&[(name, "value")]), &Environment::new()).is_err());
        }
        assert!(render_exports(&BTreeMap::from([("VALUE".into(), "a\0b".into())])).is_err());
        assert!(render_exports(&BTreeMap::from([("BAD-NAME".into(), "value".into())])).is_err());
    }

    #[test]
    fn detects_generated_name_collisions() {
        let mut cfg = config(&[]);
        cfg.gits = ["parent/repo", "other/repo"]
            .iter()
            .map(|name| GitConfig {
                name: name.to_string(),
                ..Default::default()
            })
            .collect();
        assert!(resolve(&cfg, &Environment::new())
            .unwrap_err()
            .contains("conflict"));
    }

    #[test]
    fn renders_sorted_and_quotes_without_interpreting_data() {
        let output = render_exports(&BTreeMap::from([
            ("Z".into(), String::new()),
            ("A".into(), "a'b\n$(`literal`);\\\"".into()),
        ]))
        .unwrap();
        assert_eq!(
            output,
            "export A='a'\\''b\n$(`literal`);\\\"'\nexport Z=''\n"
        );
    }

    #[test]
    fn deeply_nested_definitions_do_not_recurse_on_the_call_stack() {
        let mut definitions = BTreeMap::new();
        for i in 0..2000 {
            definitions.insert(
                format!("V{i:04}"),
                vec![Part::Reference(format!("V{:04}", i + 1))],
            );
        }
        definitions.insert("V2000".into(), vec![Part::Literal("end".into())]);
        assert_eq!(resolve_definitions(&definitions).unwrap()["V0000"], "end");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_referenced_non_unicode_environment_but_ignores_unrelated_values() {
        use std::os::unix::ffi::OsStringExt;
        let env = HashMap::from([(OsString::from("HOST"), OsString::from_vec(vec![0xff]))]);
        assert!(resolve(&config(&[("VALUE", "$HOST")]), &env).is_err());
        assert!(resolve(&config(&[("HOST", "default")]), &env).is_err());
        assert!(resolve(&config(&[]), &env).is_ok());
    }
}
