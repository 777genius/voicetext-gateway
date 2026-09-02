//! Fail-closed resolution of first-party Rust source edges.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::Boundary;
mod unsupported;
pub(super) use unsupported::reject_unsupported_constructs;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    ColonColon,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Pound,
    Bang,
    Comma,
    Star,
    Semicolon,
    Other,
}

#[derive(Debug)]
struct ModuleIndex {
    modules: BTreeMap<(PathBuf, Vec<String>), PathBuf>,
    crates: BTreeMap<String, PathBuf>,
}

pub(super) fn enforce(
    boundaries: &[Boundary],
    source_roots: &[PathBuf],
    source_files: &[PathBuf],
    repository_root: &Path,
    errors: &mut Vec<String>,
) {
    let index = ModuleIndex::new(source_roots, source_files, errors);
    for relative_file in source_files {
        let Ok(source) = fs::read_to_string(repository_root.join(relative_file)) else {
            continue;
        };
        enforce_file(boundaries, &index, relative_file, &source, errors);
    }
}

impl ModuleIndex {
    fn new(source_roots: &[PathBuf], source_files: &[PathBuf], errors: &mut Vec<String>) -> Self {
        let mut modules = BTreeMap::new();
        let mut crates = BTreeMap::new();
        for root in source_roots {
            let Some(package) = root.parent().and_then(Path::file_name) else {
                continue;
            };
            let crate_name = package.to_string_lossy().replace('-', "_");
            if let Some(existing) = crates.insert(crate_name.clone(), root.clone())
                && existing != *root
            {
                errors.push(format!(
                    "ambiguous first-party crate name `{crate_name}` for {} and {}",
                    existing.display(),
                    root.display()
                ));
            }
        }
        for file in source_files {
            let Some(root) = source_roots.iter().find(|root| file.starts_with(root)) else {
                continue;
            };
            if let Some(module) = module_path(root, file) {
                if let Some(existing) = modules.insert((root.clone(), module.clone()), file.clone())
                    && existing != *file
                    && (!module.is_empty() || !standard_root_pair(&existing, file))
                {
                    errors.push(format!(
                        "ambiguous first-party module `{}` for {} and {}",
                        module.join("::"),
                        existing.display(),
                        file.display()
                    ));
                }
            }
        }
        Self { modules, crates }
    }

    fn source_root(&self, file: &Path) -> Option<&PathBuf> {
        self.crates.values().find(|root| file.starts_with(root))
    }

    fn resolve(&self, file: &Path, path: &[String]) -> Result<Option<&PathBuf>, String> {
        let Some(current_root) = self.source_root(file) else {
            return Ok(None);
        };
        let (root, mut module, consumed) = match path.first().map(String::as_str) {
            Some("crate") => (current_root, Vec::new(), 1),
            Some("self") => (
                current_root,
                module_path(current_root, file).unwrap_or_default(),
                1,
            ),
            Some("super") => {
                let current = module_path(current_root, file).unwrap_or_default();
                let count = path.iter().take_while(|part| *part == "super").count();
                if count > current.len() {
                    return Err(format!(
                        "first-party path `{}` escapes its crate root",
                        path.join("::")
                    ));
                }
                let mut candidate = current[..current.len() - count].to_vec();
                candidate.extend(path[count..].iter().cloned());
                return self
                    .resolve_module(current_root, &candidate)
                    .map(|(target, _)| Some(target))
                    .ok_or_else(|| {
                        format!("cannot resolve first-party path `{}`", path.join("::"))
                    });
            }
            Some(first) => {
                let Some(root) = self.crates.get(first) else {
                    return Ok(None);
                };
                (root, Vec::new(), 1)
            }
            None => return Ok(None),
        };
        module.extend(path[consumed..].iter().cloned());
        if let Some((target, _)) = self.resolve_module(root, &module) {
            return Ok(Some(target));
        }
        Err(format!(
            "cannot resolve first-party path `{}`",
            path.join("::")
        ))
    }

    fn resolve_module<'a>(
        &'a self,
        root: &Path,
        module: &[String],
    ) -> Option<(&'a PathBuf, usize)> {
        for length in (0..=module.len()).rev() {
            if let Some(target) = self
                .modules
                .get(&(root.to_path_buf(), module[..length].to_vec()))
            {
                return Some((target, length));
            }
        }
        None
    }
}

fn standard_root_pair(left: &Path, right: &Path) -> bool {
    let names = [left.file_name(), right.file_name()];
    names.contains(&Some(std::ffi::OsStr::new("lib.rs")))
        && names.contains(&Some(std::ffi::OsStr::new("main.rs")))
}

fn module_path(root: &Path, file: &Path) -> Option<Vec<String>> {
    let relative = file.strip_prefix(root).ok()?;
    let mut parts = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let file_name = parts.pop()?;
    match file_name.as_str() {
        "lib.rs" | "main.rs" | "mod.rs" => {}
        _ => parts.push(file_name.strip_suffix(".rs")?.to_owned()),
    }
    Some(parts)
}

fn enforce_file(
    boundaries: &[Boundary],
    index: &ModuleIndex,
    file: &Path,
    source: &str,
    errors: &mut Vec<String>,
) {
    let Some(source_boundary) = classify(boundaries, file) else {
        return;
    };
    let mut resolved = BTreeSet::new();
    for path in first_party_paths(source, index, file) {
        match index.resolve(file, &path) {
            Ok(Some(target)) => {
                let Some(target_boundary) = classify(boundaries, target) else {
                    errors.push(format!(
                        "{}: resolved `{}` to unclassified {}",
                        file.display(),
                        path.join("::"),
                        target.display()
                    ));
                    continue;
                };
                if target_boundary.name != source_boundary.name
                    && !source_boundary.allowed.contains(&target_boundary.name)
                    && resolved.insert(target_boundary.name.clone())
                {
                    errors.push(format!(
                        "{}: undeclared first-party edge {} -> {} via `{}`",
                        file.display(),
                        source_boundary.name,
                        target_boundary.name,
                        path.join("::")
                    ));
                }
            }
            Ok(None) => {}
            Err(message) => errors.push(format!("{}: {message}", file.display())),
        }
    }
}

fn classify<'a>(boundaries: &'a [Boundary], file: &Path) -> Option<&'a Boundary> {
    let mut matches = boundaries
        .iter()
        .filter(|boundary| file.starts_with(&boundary.root));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn first_party_paths(source: &str, index: &ModuleIndex, file: &Path) -> Vec<Vec<String>> {
    let source = without_cfg_test_modules(source);
    let tokens = tokenize(&source);
    let mut paths = use_paths(&tokens);
    let mut index_token = 0;
    while index_token < tokens.len() {
        if matches!(&tokens[index_token], Token::Ident(value) if value == "mod")
            && let (Some(Token::Ident(module)), Some(Token::Semicolon)) =
                (tokens.get(index_token + 1), tokens.get(index_token + 2))
        {
            let mut path = vec!["self".to_owned()];
            path.push(module.clone());
            paths.push(path);
        }
        let Token::Ident(first) = &tokens[index_token] else {
            index_token += 1;
            continue;
        };
        let recognized = matches!(first.as_str(), "crate" | "self" | "super")
            || index.crates.contains_key(first);
        if !recognized || tokens.get(index_token + 1) != Some(&Token::ColonColon) {
            index_token += 1;
            continue;
        }
        let mut path = vec![first.clone()];
        let mut cursor = index_token + 1;
        while tokens.get(cursor) == Some(&Token::ColonColon) {
            let Some(Token::Ident(part)) = tokens.get(cursor + 1) else {
                break;
            };
            path.push(part.clone());
            cursor += 2;
        }
        if path.len() > 1 {
            paths.push(path);
        }
        index_token = cursor.max(index_token + 1);
    }

    // A root module can declare itself with `mod`; resolving `self::name` from
    // the root works, while declarations inside `foo.rs` are `foo::name`.
    let current_module = index
        .source_root(file)
        .and_then(|root| module_path(root, file))
        .unwrap_or_default();
    for path in &mut paths {
        if path.first().is_some_and(|part| part == "self") && !current_module.is_empty() {
            path.splice(
                0..1,
                std::iter::once("crate".to_owned()).chain(current_module.clone()),
            );
        }
    }
    paths
}

fn without_cfg_test_modules(source: &str) -> String {
    let mut output = source.as_bytes().to_vec();
    let mut search = 0;
    while let Some(offset) = source[search..].find("#[cfg(test)]") {
        let start = search + offset;
        let remainder = &source[start..];
        let semicolon = remainder.find(';');
        let opening = remainder.find('{');
        if let Some(semicolon) = semicolon
            && opening.is_none_or(|opening| semicolon < opening)
        {
            let end = start + semicolon + 1;
            for byte in &mut output[start..end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            search = end;
            continue;
        }
        let Some(open_offset) = opening else {
            break;
        };
        let open = start + open_offset;
        let mut depth = 0_u32;
        let mut end = open;
        for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end == open {
            break;
        }
        for byte in &mut output[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        search = end;
    }
    String::from_utf8(output).expect("source began as UTF-8")
}

fn use_paths(tokens: &[Token]) -> Vec<Vec<String>> {
    let mut paths = Vec::new();
    let mut cursor = 0;
    while cursor < tokens.len() {
        if matches!(&tokens[cursor], Token::Ident(value) if value == "use") {
            cursor += 1;
            parse_use_tree(tokens, &mut cursor, &[], &mut paths);
        } else {
            cursor += 1;
        }
    }
    paths
}

fn parse_use_tree(
    tokens: &[Token],
    cursor: &mut usize,
    prefix: &[String],
    paths: &mut Vec<Vec<String>>,
) {
    let mut path = prefix.to_vec();
    while let Some(token) = tokens.get(*cursor) {
        match token {
            Token::Ident(part) if part == "self" && !path.is_empty() => {
                *cursor += 1;
                paths.push(path);
                return;
            }
            Token::Ident(part) => {
                path.push(part.clone());
                *cursor += 1;
                if tokens.get(*cursor) == Some(&Token::ColonColon) {
                    *cursor += 1;
                } else {
                    paths.push(path);
                    skip_alias(tokens, cursor);
                    return;
                }
            }
            Token::LeftBrace => {
                *cursor += 1;
                while *cursor < tokens.len() && tokens[*cursor] != Token::RightBrace {
                    parse_use_tree(tokens, cursor, &path, paths);
                    if tokens.get(*cursor) == Some(&Token::Comma) {
                        *cursor += 1;
                    }
                }
                if tokens.get(*cursor) == Some(&Token::RightBrace) {
                    *cursor += 1;
                }
                return;
            }
            Token::Star => {
                *cursor += 1;
                paths.push(path);
                return;
            }
            _ => {
                if !path.is_empty() {
                    paths.push(path);
                }
                return;
            }
        }
    }
}

fn skip_alias(tokens: &[Token], cursor: &mut usize) {
    if matches!(tokens.get(*cursor), Some(Token::Ident(value)) if value == "as") {
        *cursor += 1;
        if matches!(tokens.get(*cursor), Some(Token::Ident(_))) {
            *cursor += 1;
        }
    }
}

fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        } else if bytes[cursor..].starts_with(b"//") {
            cursor = skip_line(bytes, cursor + 2);
        } else if bytes[cursor..].starts_with(b"/*") {
            cursor = skip_block(bytes, cursor + 2);
        } else if bytes[cursor] == b'"' {
            cursor = skip_quoted(bytes, cursor + 1, b'"');
        } else if bytes[cursor] == b'\'' {
            if bytes
                .get(cursor + 1)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                && bytes.get(cursor + 2) != Some(&b'\'')
            {
                tokens.push(Token::Other);
                cursor += 1;
            } else {
                cursor = skip_quoted(bytes, cursor + 1, b'\'');
            }
        } else if bytes[cursor] == b'r' && raw_string_hashes(bytes, cursor).is_some() {
            cursor = skip_raw_string(bytes, cursor);
        } else if bytes[cursor..].starts_with(b"::") {
            tokens.push(Token::ColonColon);
            cursor += 2;
        } else if bytes[cursor] == b'{' {
            tokens.push(Token::LeftBrace);
            cursor += 1;
        } else if bytes[cursor] == b'}' {
            tokens.push(Token::RightBrace);
            cursor += 1;
        } else if bytes[cursor] == b'[' {
            tokens.push(Token::LeftBracket);
            cursor += 1;
        } else if bytes[cursor] == b']' {
            tokens.push(Token::RightBracket);
            cursor += 1;
        } else if bytes[cursor] == b'#' {
            tokens.push(Token::Pound);
            cursor += 1;
        } else if bytes[cursor] == b'!' {
            tokens.push(Token::Bang);
            cursor += 1;
        } else if bytes[cursor] == b',' {
            tokens.push(Token::Comma);
            cursor += 1;
        } else if bytes[cursor] == b'*' {
            tokens.push(Token::Star);
            cursor += 1;
        } else if bytes[cursor] == b';' {
            tokens.push(Token::Semicolon);
            cursor += 1;
        } else if bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_' {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
            {
                cursor += 1;
            }
            tokens.push(Token::Ident(source[start..cursor].to_owned()));
        } else {
            tokens.push(Token::Other);
            cursor += 1;
        }
    }
    tokens
}

fn skip_line(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

fn skip_block(bytes: &[u8], mut cursor: usize) -> usize {
    let mut depth = 1_u32;
    while cursor < bytes.len() && depth > 0 {
        if bytes[cursor..].starts_with(b"/*") {
            depth += 1;
            cursor += 2;
        } else if bytes[cursor..].starts_with(b"*/") {
            depth -= 1;
            cursor += 2;
        } else {
            cursor += 1;
        }
    }
    cursor
}

fn skip_quoted(bytes: &[u8], mut cursor: usize, quote: u8) -> usize {
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(bytes.len());
        } else if bytes[cursor] == quote {
            return cursor + 1;
        } else {
            cursor += 1;
        }
    }
    cursor
}

fn raw_string_hashes(bytes: &[u8], cursor: usize) -> Option<usize> {
    let mut probe = cursor + 1;
    while bytes.get(probe) == Some(&b'#') {
        probe += 1;
    }
    (bytes.get(probe) == Some(&b'"')).then_some(probe - cursor - 1)
}

fn skip_raw_string(bytes: &[u8], cursor: usize) -> usize {
    let hashes = raw_string_hashes(bytes, cursor).unwrap_or(0);
    let mut probe = cursor + hashes + 2;
    while probe < bytes.len() {
        if bytes[probe] == b'"'
            && bytes.get(probe + 1..probe + 1 + hashes)
                == Some(&bytes[cursor + 1..cursor + 1 + hashes])
        {
            return probe + hashes + 1;
        }
        probe += 1;
    }
    probe
}

#[cfg(test)]
mod tests;
