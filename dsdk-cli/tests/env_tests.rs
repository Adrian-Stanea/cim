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

use dsdk_cli::config::{GitConfig, SdkConfig};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::{tempdir, TempDir};

fn config(vars: &[(&str, &str)]) -> SdkConfig {
    SdkConfig {
        variables: Some(
            vars.iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        ),
        ..Default::default()
    }
}

fn workspace(config: &SdkConfig) -> TempDir {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("sdk.yml"),
        noyalib::to_string(config).unwrap(),
    )
    .unwrap();
    fs::write(dir.path().join(".workspace"), "").unwrap();
    dir
}

fn command(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cim"));
    command.arg("env").current_dir(dir).env_clear();
    command
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

fn failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(expected), "{stderr}");
}

#[test]
fn exports_from_nested_directory_without_makefiles_or_side_effects() {
    let dir = workspace(&config(&[
        ("SDK_ARCH", "aarch64"),
        ("SDK_BIN", "${{ WORKSPACE }}/bin"),
        ("SDK_COPY", "${{ SDK_BIN }}"),
    ]));
    let nested = dir.path().join("repo/src");
    fs::create_dir_all(&nested).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let output = success(command(&nested).output().unwrap());
    assert_eq!(
        output,
        format!(
            "export SDK_ARCH='aarch64'\nexport SDK_BIN='{0}/bin'\nexport SDK_COPY='{0}/bin'\nexport WORKSPACE='{0}'\n",
            root.display()
        )
    );
    let mut entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, [".workspace", "repo", "sdk.yml"]);
}

#[test]
fn loads_local_inheritance_and_variable_overlay() {
    let dir = workspace(&config(&[]));
    let overlays = dir.path().join(".cim/target-overlays");
    fs::create_dir_all(&overlays).unwrap();
    fs::write(
        overlays.join("base-sdk.yml"),
        "gits: []\nvariables:\n  SDK_BASE: original\n  SDK_REMOVE: removed\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("sdk.yml"),
        "extends: base\ngits: []\nvariables:\n  SDK_OWN: '${{ SDK_BASE }}/own'\noverlay:\n  variables:\n    remove: [SDK_REMOVE]\n    set:\n      SDK_BASE: modified\n",
    )
    .unwrap();
    let output = success(command(dir.path()).output().unwrap());
    assert!(output.contains("export SDK_BASE='modified'\n"));
    assert!(output.contains("export SDK_OWN='modified/own'\n"));
    assert!(!output.contains("SDK_REMOVE"));
    fs::remove_file(overlays.join("base-sdk.yml")).unwrap();
    failure(command(dir.path()).output().unwrap(), "not found");
}

#[test]
fn ignores_clone_filters_and_stale_generated_environment_paths() {
    let mut cfg = config(&[("SDK_EXPLICIT", "default"), ("SDK_EMPTY", "default")]);
    cfg.gits = vec![
        GitConfig {
            name: "nested/repo".to_string(),
            python_deps: Some(vec!["requirements.txt".to_string()]),
            group: Some(vec!["excluded".to_string()]),
            ..Default::default()
        },
        GitConfig {
            name: "other".to_string(),
            ..Default::default()
        },
    ];
    let dir = workspace(&cfg);
    fs::write(
        dir.path().join(".workspace"),
        "match_pattern: never-match\nexclude_groups: excluded\n",
    )
    .unwrap();
    let output = success(
        command(dir.path())
            .env("WORKSPACE", "/stale")
            .env("REPO_DIR", "/stale/repo")
            .env("REPO_VENV", "/stale/venv")
            .env("SDK_EXPLICIT", "override")
            .env("SDK_EMPTY", "")
            .output()
            .unwrap(),
    );
    let root = dir.path().canonicalize().unwrap();
    assert!(output.contains(&format!(
        "export REPO_DIR='{}/nested/repo'\n",
        root.display()
    )));
    assert!(output.contains(&format!(
        "export REPO_VENV='{}/.cim/nested/repo/.venv'\n",
        root.display()
    )));
    assert!(output.contains("export OTHER_DIR="));
    assert!(!output.contains("OTHER_VENV"));
    assert!(output.contains("export SDK_EXPLICIT='override'\n"));
    assert!(output.contains("export SDK_EMPTY=''\n"));
    assert!(!output.contains("/stale"));
}

#[test]
fn relocation_and_manifest_edits_take_effect_without_regeneration() {
    let parent = tempdir().unwrap();
    let dir = workspace(&config(&[("SDK_PATH", "${{ WORKSPACE }}/bin")]));
    let moved = parent.path().join("moved workspace");
    fs::rename(dir.path(), &moved).unwrap();
    let output = success(command(&moved).output().unwrap());
    assert!(output.contains(&format!(
        "export SDK_PATH='{}/bin'\n",
        moved.canonicalize().unwrap().display()
    )));
    let mut cfg = config(&[("SDK_PATH", "${{ WORKSPACE }}/updated")]);
    cfg.variables
        .get_or_insert_with(HashMap::new)
        .insert("SDK_ADDED".into(), "new".into());
    fs::write(moved.join("sdk.yml"), noyalib::to_string(&cfg).unwrap()).unwrap();
    let output = success(command(&moved).output().unwrap());
    assert!(output.contains("/updated'\n"));
    assert!(output.contains("export SDK_ADDED='new'\n"));
}

#[test]
fn invalid_definitions_produce_no_partial_exports() {
    for (vars, expected) in [
        (vec![("Z_BAD", "$MISSING")], "MISSING"),
        (vec![("Z_BAD", "${{ MISSING }}")], "Z_BAD -> MISSING"),
        (
            vec![("Z_BAD", "${{ Z_CYCLE }}"), ("Z_CYCLE", "${{ Z_BAD }}")],
            "cycle",
        ),
        (vec![("Z_BAD", "$(WORKSPACE)")], "unsupported"),
        (vec![("Z_BAD", "${{ BROKEN")], "Unclosed"),
        (vec![("Z_BAD", "has\0nul")], "NUL"),
        (vec![("INVALID-NAME", "data")], "Invalid POSIX"),
        (vec![("WORKSPACE", "/override")], "reserved"),
    ] {
        let mut cfg = config(&vars);
        cfg.variables
            .get_or_insert_with(HashMap::new)
            .insert("A_VALID".into(), "must not be printed".into());
        let dir = workspace(&cfg);
        failure(command(dir.path()).output().unwrap(), expected);
    }
}

#[test]
fn workspace_and_manifest_failures_have_nonzero_status_and_empty_stdout() {
    let dir = tempdir().unwrap();
    failure(command(dir.path()).output().unwrap(), "Not in a workspace");
    fs::write(dir.path().join(".workspace"), "").unwrap();
    failure(command(dir.path()).output().unwrap(), "sdk.yml not found");
    fs::write(dir.path().join("sdk.yml"), "gits: [").unwrap();
    failure(command(dir.path()).output().unwrap(), "Cannot export");
}

#[test]
fn command_is_discoverable_and_rejects_unsupported_options() {
    let help = Command::new(env!("CARGO_BIN_EXE_cim"))
        .args(["env", "--help"])
        .output()
        .unwrap();
    let help = success(help);
    assert!(help.contains("POSIX"));
    assert!(help.contains("Usage: cim env"));
    let dir = workspace(&config(&[]));
    failure(
        command(dir.path())
            .arg("--shell")
            .arg("fish")
            .output()
            .unwrap(),
        "unexpected argument",
    );
}

#[cfg(unix)]
#[test]
fn exports_round_trip_through_posix_shell_as_literal_data() {
    let values = [
        ("SDK_EMPTY", ""),
        ("SDK_QUOTES", "single' double\" backslash\\"),
        ("SDK_LINES", "first\nsecond\n\n"),
        ("SDK_UNICODE", "\u{00e9}\u{1f680}"),
        ("SDK_HOST", "ignored"),
    ];
    let dir = workspace(&config(&values));
    // Explicit escapes avoid the YAML serializer's multiline block formatting.
    fs::write(
        dir.path().join("sdk.yml"),
        "gits: []\nvariables:\n  SDK_EMPTY: ''\n  SDK_QUOTES: \"single' double\\\" backslash\\\\\"\n  SDK_LINES: \"first\\nsecond\\n\\n\"\n  SDK_UNICODE: \"\\u00e9\\U0001f680\"\n  SDK_HOST: ignored\n",
    )
    .unwrap();
    let payload =
        "'; touch injected; # $(touch injected) `touch injected` $MISSING %HOST% ${{ UNKNOWN }}";
    let output = success(
        command(dir.path())
            .env("SDK_HOST", payload)
            .output()
            .unwrap(),
    );
    let exports = dir.path().join("exports.sh");
    fs::write(&exports, output).unwrap();
    for (name, expected) in values
        .iter()
        .map(|&(name, value)| (name, if name == "SDK_HOST" { payload } else { value }))
    {
        // A child process checks that values are exported, not just shell-local.
        let output = Command::new("sh")
            .args([
                "-c",
                ". \"$1\"; exec sh -c 'printenv \"$1\"' sh \"$2\"",
                "sh",
            ])
            .arg(&exports)
            .arg(name)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, format!("{expected}\n").as_bytes());
    }
    assert!(!dir.path().join("injected").exists());
}

#[cfg(unix)]
#[test]
fn referenced_non_unicode_values_fail_without_stdout() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let dir = workspace(&config(&[("SDK_VALUE", "$HOST")]));
    failure(
        command(dir.path())
            .env("HOST", OsString::from_vec(vec![0xff]))
            .output()
            .unwrap(),
        "not valid Unicode",
    );
}
