use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod source_dependencies;

const MAX_SOURCE_LINES: usize = 600;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Boundary {
    name: String,
    root: PathBuf,
    allowed: Vec<String>,
}

fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() != ["verify"] {
        eprintln!("usage: cargo run -p xtask -- verify");
        return ExitCode::from(2);
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(repository_root) = manifest_dir.parent() else {
        eprintln!("verification failed: xtask manifest has no repository parent");
        return ExitCode::FAILURE;
    };

    match verify(repository_root) {
        Ok(()) => {
            println!("source dependency verification passed");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!("source dependency verification failed:");
            for error in errors {
                eprintln!("- {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn verify(repository_root: &Path) -> Result<(), Vec<String>> {
    let architecture_path = repository_root.join("architecture/source-dependencies.txt");
    let architecture = fs::read_to_string(&architecture_path).map_err(|error| {
        vec![format!(
            "cannot read {}: {error}",
            architecture_path.display()
        )]
    })?;
    let boundaries = parse_boundaries(&architecture)?;
    let mut errors = validate_boundaries(repository_root, &boundaries);
    validate_public_tls_surface(repository_root, &mut errors);

    let manifest_path = repository_root.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|error| vec![format!("cannot read {}: {error}", manifest_path.display())])?;
    if manifest.lines().any(|line| line.contains("package =")) {
        errors.push("workspace dependencies cannot rename first-party crates".to_owned());
    }
    let members = parse_workspace_members(&manifest)?;
    let source_roots = workspace_source_roots(repository_root, &members, &mut errors);
    let source_files = enumerate_rust_files(repository_root, &source_roots, &mut errors);
    let test_roots = workspace_test_roots(repository_root, &members);
    let test_files = enumerate_rust_files(repository_root, &test_roots, &mut errors);

    source_dependencies::enforce(
        &boundaries,
        &source_roots,
        &source_files,
        repository_root,
        &mut errors,
    );

    for relative_file in source_files.into_iter().chain(test_files) {
        verify_source_file(repository_root, &relative_file, &boundaries, &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_public_tls_surface(repository_root: &Path, errors: &mut Vec<String>) {
    let path = repository_root.join("deploy/Caddyfile");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", path.display()));
            return;
        }
    };
    for route in [
        "/api/v1/transcribe/batch",
        "/api/v1/transcribe/batch/*",
        "/api/v1/transcribe/stream",
        "/health",
        "/health/live",
        "/health/ready",
    ] {
        if !source
            .lines()
            .any(|line| line.trim() == route || line.trim() == format!("{route} \\"))
        {
            errors.push(format!(
                "deploy/Caddyfile does not expose required route `{route}`"
            ));
        }
    }
    if !source.contains("reverse_proxy @voicetext_contract gateway:8080")
        || !source.lines().any(|line| line.trim() == "respond 404")
        || source.lines().any(|line| line.trim() == "/metrics")
    {
        errors.push("deploy/Caddyfile public route allowlist is unsafe".to_owned());
    }
}

fn parse_boundaries(input: &str) -> Result<Vec<Boundary>, Vec<String>> {
    let mut boundaries = Vec::new();
    let mut errors = Vec::new();
    let mut names = HashSet::new();
    let mut roots = HashSet::new();

    for (index, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('|').map(str::trim).collect::<Vec<_>>();
        if fields.len() != 3 || fields[0].is_empty() || fields[1].is_empty() {
            errors.push(format!(
                "architecture/source-dependencies.txt:{} must be boundary|root|allowed",
                index + 1
            ));
            continue;
        }
        if !names.insert(fields[0].to_owned()) {
            errors.push(format!("duplicate boundary `{}`", fields[0]));
        }
        let root = PathBuf::from(fields[1]);
        if root.is_absolute() || fields[1].split('/').any(|part| part == "..") {
            errors.push(format!("boundary `{}` has an unsafe root", fields[0]));
        }
        if !roots.insert(root.clone()) {
            errors.push(format!("duplicate classification root `{}`", fields[1]));
        }
        let allowed = fields[2]
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        boundaries.push(Boundary {
            name: fields[0].to_owned(),
            root,
            allowed,
        });
    }

    if boundaries.is_empty() {
        errors.push("source dependency model contains no boundaries".to_owned());
    }
    let known = boundaries
        .iter()
        .map(|boundary| boundary.name.as_str())
        .collect::<HashSet<_>>();
    for boundary in &boundaries {
        for allowed in &boundary.allowed {
            if allowed == &boundary.name || !known.contains(allowed.as_str()) {
                errors.push(format!(
                    "boundary `{}` has invalid allowed boundary `{allowed}`",
                    boundary.name
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(boundaries)
    } else {
        Err(errors)
    }
}

fn parse_workspace_members(manifest: &str) -> Result<Vec<PathBuf>, Vec<String>> {
    let Some(workspace_start) = manifest.find("[workspace]") else {
        return Err(vec!["Cargo.toml has no [workspace] section".to_owned()]);
    };
    let workspace = &manifest[workspace_start + "[workspace]".len()..];
    let workspace_end = workspace.find("\n[").unwrap_or(workspace.len());
    let workspace = &workspace[..workspace_end];
    let Some(members_start) = workspace.find("members") else {
        return Err(vec!["[workspace] has no members list".to_owned()]);
    };
    let members = &workspace[members_start + "members".len()..];
    let Some(opening) = members.find('[') else {
        return Err(vec![
            "workspace members list has no opening bracket".to_owned(),
        ]);
    };
    let Some(closing) = members[opening + 1..].find(']') else {
        return Err(vec![
            "workspace members list has no closing bracket".to_owned(),
        ]);
    };
    let list = &members[opening + 1..opening + 1 + closing];
    let mut values = Vec::new();
    let mut remainder = list;
    while let Some(start) = remainder.find('"') {
        remainder = &remainder[start + 1..];
        let Some(end) = remainder.find('"') else {
            return Err(vec![
                "workspace member has an unterminated quote".to_owned(),
            ]);
        };
        let member = &remainder[..end];
        if member.is_empty() || member.contains(['*', '?']) || Path::new(member).is_absolute() {
            return Err(vec![format!("unsupported workspace member `{member}`")]);
        }
        values.push(PathBuf::from(member));
        remainder = &remainder[end + 1..];
    }
    if values.is_empty() {
        Err(vec!["workspace members list is empty".to_owned()])
    } else {
        Ok(values)
    }
}

fn validate_boundaries(repository_root: &Path, boundaries: &[Boundary]) -> Vec<String> {
    boundaries
        .iter()
        .filter_map(|boundary| {
            let path = repository_root.join(&boundary.root);
            (!path.exists()).then(|| {
                format!(
                    "boundary `{}` root does not exist: {}",
                    boundary.name,
                    boundary.root.display()
                )
            })
        })
        .collect()
}

fn workspace_source_roots(
    repository_root: &Path,
    members: &[PathBuf],
    errors: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for member in members {
        validate_member_manifest(repository_root, member, errors);
        let source_root = member.join("src");
        let absolute = repository_root.join(&source_root);
        if absolute.is_dir() {
            roots.insert(source_root);
        } else {
            errors.push(format!(
                "workspace member `{}` has no src directory",
                member.display()
            ));
        }
    }
    roots.insert(PathBuf::from("xtask/src"));
    roots.into_iter().collect()
}

fn workspace_test_roots(repository_root: &Path, members: &[PathBuf]) -> Vec<PathBuf> {
    members
        .iter()
        .map(|member| member.join("tests"))
        .filter(|root| repository_root.join(root).is_dir())
        .collect()
}

fn validate_member_manifest(repository_root: &Path, member: &Path, errors: &mut Vec<String>) {
    let manifest_path = member.join("Cargo.toml");
    let absolute = repository_root.join(&manifest_path);
    let manifest = match fs::read_to_string(&absolute) {
        Ok(manifest) => manifest,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", manifest_path.display()));
            return;
        }
    };
    let expected = member
        .file_name()
        .map(|name| name.to_string_lossy().replace('-', "_"));
    validate_manifest_text(&manifest_path, &manifest, expected.as_deref(), errors);
    let build_script = member.join("build.rs");
    if repository_root.join(&build_script).exists() {
        errors.push(format!(
            "unsupported first-party build script {}",
            build_script.display()
        ));
    }
    for unsupported in ["examples", "benches", "src/bin"] {
        let target = member.join(unsupported);
        if repository_root.join(&target).exists() {
            errors.push(format!(
                "unsupported implicit Cargo target directory {}",
                target.display()
            ));
        }
    }
}

fn validate_manifest_text(
    manifest_path: &Path,
    manifest: &str,
    expected: Option<&str>,
    errors: &mut Vec<String>,
) {
    let actual = manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("name = \"")?.strip_suffix('"'))
        .map(|name| name.replace('-', "_"));
    if actual.as_deref() != expected {
        errors.push(format!(
            "{} package name must match its workspace directory",
            manifest_path.display()
        ));
    }
    for unsupported in ["[lib]", "[[bin]]", "[[example]]", "[[test]]", "[[bench]]"] {
        if manifest.lines().any(|line| line.trim() == unsupported) {
            errors.push(format!(
                "{} uses unsupported custom Cargo target `{unsupported}`",
                manifest_path.display()
            ));
        }
    }
    if manifest.lines().any(|line| {
        let line = line.trim();
        line.starts_with("build =") || line.contains("package =")
    }) {
        errors.push(format!(
            "{} uses an unsupported build script or dependency alias",
            manifest_path.display()
        ));
    }
}

fn enumerate_rust_files(
    repository_root: &Path,
    roots: &[PathBuf],
    errors: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut files = BTreeSet::new();
    for root in roots {
        visit_directory(repository_root, root, &mut files, errors);
    }
    files.into_iter().collect()
}

fn visit_directory(
    repository_root: &Path,
    relative_directory: &Path,
    files: &mut BTreeSet<PathBuf>,
    errors: &mut Vec<String>,
) {
    let directory = repository_root.join(relative_directory);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!(
                "cannot read {}: {error}",
                relative_directory.display()
            ));
            return;
        }
    };
    let mut paths = entries.filter_map(Result::ok).collect::<Vec<_>>();
    paths.sort_by_key(fs::DirEntry::path);
    for entry in paths {
        let path = entry.path();
        let relative = match path.strip_prefix(repository_root) {
            Ok(relative) => relative.to_path_buf(),
            Err(error) => {
                errors.push(format!("cannot relativize {}: {error}", path.display()));
                continue;
            }
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push(format!("cannot inspect {}: {error}", relative.display()));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            errors.push(format!(
                "source tree contains symlink: {}",
                relative.display()
            ));
        } else if metadata.is_dir() {
            visit_directory(repository_root, &relative, files, errors);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.insert(relative);
        }
    }
}

fn verify_source_file(
    repository_root: &Path,
    relative_file: &Path,
    boundaries: &[Boundary],
    errors: &mut Vec<String>,
) {
    let matches = boundaries
        .iter()
        .filter(|boundary| relative_file.starts_with(&boundary.root))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        let names = matches
            .iter()
            .map(|boundary| boundary.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        errors.push(format!(
            "{} has {} boundary classifications{}",
            relative_file.display(),
            matches.len(),
            if names.is_empty() {
                String::new()
            } else {
                format!(": {names}")
            }
        ));
        return;
    }
    let source = match fs::read_to_string(repository_root.join(relative_file)) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", relative_file.display()));
            return;
        }
    };
    let line_count = source.lines().count();
    if line_count > MAX_SOURCE_LINES {
        errors.push(format!(
            "{} has {line_count} lines; maximum is {MAX_SOURCE_LINES}",
            relative_file.display()
        ));
    }
    enforce_directional_rules(relative_file, matches[0], &source, errors);
}

fn enforce_directional_rules(
    relative_file: &Path,
    boundary: &Boundary,
    source: &str,
    errors: &mut Vec<String>,
) {
    source_dependencies::reject_unsupported_constructs(relative_file, source, errors);
    let is_domain = boundary.name.ends_with(".domain");
    let is_application = boundary.name.ends_with(".application");
    if !is_domain && !is_application {
        return;
    }
    let code = without_comments(source);
    let compact = code
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if is_domain && compact.contains("application::") {
        errors.push(format!(
            "{}: domain cannot import application",
            relative_file.display()
        ));
    }

    let identifiers = code
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|identifier| !identifier.is_empty())
        .collect::<Vec<_>>();
    for forbidden in ["tokio", "axum", "sqlx", "reqwest"] {
        if identifiers.contains(&forbidden) {
            errors.push(format!(
                "{}: `{forbidden}` is forbidden in {}",
                relative_file.display(),
                boundary.name
            ));
        }
    }
    if compact.contains("std::env") {
        errors.push(format!(
            "{}: `std::env` is forbidden in {}",
            relative_file.display(),
            boundary.name
        ));
    }
    if identifiers.contains(&"SystemTime") {
        errors.push(format!(
            "{}: `SystemTime` is forbidden in {}",
            relative_file.display(),
            boundary.name
        ));
    }
    for provider in ["deepgram", "elevenlabs", "pipecat"] {
        if identifiers
            .iter()
            .any(|identifier| identifier.to_ascii_lowercase().contains(provider))
        {
            errors.push(format!(
                "{}: provider name `{provider}` is forbidden in {}",
                relative_file.display(),
                boundary.name
            ));
        }
    }
}

fn without_comments(source: &str) -> String {
    let characters = source.as_bytes();
    let mut output = Vec::with_capacity(characters.len());
    let mut index = 0;
    let mut block_depth = 0_u32;
    let mut in_string = false;
    let mut in_character = false;
    let mut escaped = false;

    while index < characters.len() {
        let current = characters[index];
        let next = characters.get(index + 1).copied();
        if block_depth > 0 {
            if current == b'/' && next == Some(b'*') {
                block_depth += 1;
                output.extend_from_slice(b"  ");
                index += 2;
            } else if current == b'*' && next == Some(b'/') {
                block_depth -= 1;
                output.extend_from_slice(b"  ");
                index += 2;
            } else {
                output.push(if current == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
        } else if !in_string && !in_character && current == b'/' && next == Some(b'*') {
            block_depth = 1;
            output.extend_from_slice(b"  ");
            index += 2;
        } else if !in_string && !in_character && current == b'/' && next == Some(b'/') {
            while index < characters.len() && characters[index] != b'\n' {
                output.push(b' ');
                index += 1;
            }
        } else {
            output.push(current);
            if escaped {
                escaped = false;
            } else if (in_string || in_character) && current == b'\\' {
                escaped = true;
            } else if !in_character && current == b'"' {
                in_string = !in_string;
            } else if !in_string && current == b'\'' {
                in_character = !in_character;
            }
            index += 1;
        }
    }
    String::from_utf8(output).expect("comment stripping preserves UTF-8 bytes")
}

#[cfg(test)]
mod tests;
