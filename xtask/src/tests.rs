use super::*;

#[test]
fn parses_current_boundary_format() {
    let boundaries = parse_boundaries(
        "# comment\nspeech.domain|crates/speech/src/domain|\n\
         speech.application|crates/speech/src/application|speech.domain\n",
    )
    .expect("valid boundaries");
    assert_eq!(boundaries.len(), 2);
    assert_eq!(boundaries[1].allowed, ["speech.domain"]);
}

#[test]
fn rejects_unknown_allowed_boundary() {
    let errors = parse_boundaries("speech.domain|src/domain|missing\n")
        .expect_err("unknown boundary must fail");
    assert!(errors.iter().any(|error| error.contains("invalid allowed")));
}

#[test]
fn parses_workspace_members() {
    let members = parse_workspace_members(
        "[workspace]\nmembers = [\n  \"crates/speech\",\n  \"xtask\",\n]\nresolver = \"3\"\n\n[workspace.package]\n",
    )
    .expect("valid workspace");
    assert_eq!(
        members,
        [PathBuf::from("crates/speech"), PathBuf::from("xtask")]
    );
}

#[test]
fn directional_rules_ignore_comments_but_reject_code() {
    let boundary = Boundary {
        name: "speech.domain".to_owned(),
        root: PathBuf::from("src/domain"),
        allowed: Vec::new(),
    };
    let mut errors = Vec::new();
    enforce_directional_rules(
        Path::new("src/domain/example.rs"),
        &boundary,
        "// use tokio;\n/* Deepgram */\nuse crate::application::Port;\n",
        &mut errors,
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("domain cannot import application"));
}

#[test]
fn adversarial_unsupported_source_constructs_fail_closed() {
    for fixture in [
        include_str!("../fixtures/source-dependencies/negative/extern_crate_alias.rs"),
        include_str!("../fixtures/source-dependencies/negative/path_module.rs"),
        include_str!("../fixtures/source-dependencies/negative/inline_module.rs"),
        include_str!("../fixtures/source-dependencies/negative/generated_source.rs"),
    ] {
        let mut errors = Vec::new();
        source_dependencies::reject_unsupported_constructs(
            Path::new("fixture.rs"),
            fixture,
            &mut errors,
        );
        assert!(!errors.is_empty(), "fixture unexpectedly passed: {fixture}");
    }
}

#[test]
fn adversarial_cargo_identity_and_target_fixtures_fail_closed() {
    for fixture in [
        include_str!("../fixtures/source-dependencies/negative/renamed_package.toml"),
        include_str!("../fixtures/source-dependencies/negative/dependency_alias.toml"),
        include_str!("../fixtures/source-dependencies/negative/custom_target.toml"),
    ] {
        let mut errors = Vec::new();
        validate_manifest_text(
            Path::new("demo/Cargo.toml"),
            fixture,
            Some("demo"),
            &mut errors,
        );
        assert!(!errors.is_empty(), "fixture unexpectedly passed: {fixture}");
    }
}

#[test]
fn directional_rules_reject_runtime_and_provider_names() {
    let boundary = Boundary {
        name: "speech.application".to_owned(),
        root: PathBuf::from("src/application"),
        allowed: vec!["speech.domain".to_owned()],
    };
    let mut errors = Vec::new();
    enforce_directional_rules(
        Path::new("src/application/example.rs"),
        &boundary,
        "use std::env; use tokio::time; struct DeepgramAdapter; type Clock = SystemTime;",
        &mut errors,
    );
    assert_eq!(errors.len(), 4);
}
