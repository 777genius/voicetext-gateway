use super::*;

fn fixture() -> (Vec<Boundary>, Vec<PathBuf>, Vec<PathBuf>, ModuleIndex) {
    let boundaries = vec![
        Boundary {
            name: "app".to_owned(),
            root: PathBuf::from("crates/demo/src/app"),
            allowed: vec!["domain".to_owned()],
        },
        Boundary {
            name: "domain".to_owned(),
            root: PathBuf::from("crates/demo/src/domain"),
            allowed: Vec::new(),
        },
        Boundary {
            name: "adapter".to_owned(),
            root: PathBuf::from("crates/demo/src/adapter"),
            allowed: vec!["app".to_owned()],
        },
    ];
    let roots = vec![PathBuf::from("crates/demo/src")];
    let files = vec![
        PathBuf::from("crates/demo/src/app/mod.rs"),
        PathBuf::from("crates/demo/src/domain/deep/model.rs"),
        PathBuf::from("crates/demo/src/adapter/deep/client.rs"),
    ];
    let index = ModuleIndex::new(&roots, &files, &mut Vec::new());
    (boundaries, roots, files, index)
}

#[test]
fn positive_fixture_resolves_deep_and_renamed_import() {
    let (boundaries, _, _, index) = fixture();
    let source = include_str!("../../fixtures/source-dependencies/positive/deep_renamed.rs");
    let mut errors = Vec::new();
    enforce_file(
        &boundaries,
        &index,
        Path::new("crates/demo/src/app/mod.rs"),
        source,
        &mut errors,
    );
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn negative_fixture_rejects_deep_and_renamed_undeclared_edge() {
    let (boundaries, _, _, index) = fixture();
    let source = include_str!("../../fixtures/source-dependencies/negative/deep_renamed.rs");
    let mut errors = Vec::new();
    enforce_file(
        &boundaries,
        &index,
        Path::new("crates/demo/src/domain/deep/model.rs"),
        source,
        &mut errors,
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("domain -> adapter"));
}

#[test]
fn tokenizer_ignores_comments_and_all_string_forms() {
    let (_, _, _, index) = fixture();
    let paths = first_party_paths(
        "// crate::adapter::Bad\n\"crate::adapter::Bad\"; r#\"crate::adapter::Bad\"#; crate::domain::Good;",
        &index,
        Path::new("crates/demo/src/app/mod.rs"),
    );
    assert_eq!(paths, [vec!["crate", "domain", "Good"]]);
}

#[test]
fn grouped_use_trees_resolve_each_deep_edge_before_aliasing() {
    let (boundaries, _, _, index) = fixture();
    let mut errors = Vec::new();
    enforce_file(
        &boundaries,
        &index,
        Path::new("crates/demo/src/domain/deep/model.rs"),
        "use crate::{domain::deep::model::Model, adapter::{deep::client::Client as C}};",
        &mut errors,
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
}

#[test]
fn exact_super_depth_cannot_resolve_from_a_nearer_module() {
    let (boundaries, _, _, index) = fixture();
    let source = include_str!("../../fixtures/source-dependencies/negative/exact_super.rs");
    let mut errors = Vec::new();
    enforce_file(
        &boundaries,
        &index,
        Path::new("crates/demo/src/domain/deep/model.rs"),
        source,
        &mut errors,
    );
    assert!(!errors.is_empty(), "{errors:?}");
    assert!(
        errors
            .iter()
            .all(|error| error.contains("cannot resolve first-party path"))
    );
}

#[test]
fn ambiguous_module_collisions_are_rejected() {
    let roots = vec![PathBuf::from("crates/demo/src")];
    let files = vec![
        PathBuf::from("crates/demo/src/domain/deep.rs"),
        PathBuf::from("crates/demo/src/domain/deep/mod.rs"),
    ];
    let mut errors = Vec::new();
    let _index = ModuleIndex::new(&roots, &files, &mut errors);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("ambiguous first-party module"));
}
