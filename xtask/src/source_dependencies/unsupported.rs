use std::path::Path;

use super::{Token, tokenize};

pub(crate) fn reject_unsupported_constructs(
    relative_file: &Path,
    source: &str,
    errors: &mut Vec<String>,
) {
    let tokens = tokenize(source);
    for (marker, present) in [
        ("#[path", attribute_contains_path(&tokens)),
        (
            "extern crate",
            tokens.windows(2).any(|pair| {
                matches!(&pair[0], Token::Ident(value) if value == "extern")
                    && matches!(&pair[1], Token::Ident(value) if value == "crate")
            }),
        ),
        (
            "include!",
            tokens.windows(2).any(|pair| {
                matches!(&pair[0], Token::Ident(value) if value == "include")
                    && pair[1] == Token::Bang
            }),
        ),
    ] {
        if present {
            errors.push(format!(
                "{} uses unsupported first-party Rust construct `{marker}`",
                relative_file.display()
            ));
        }
    }
    let mut previous = "";
    for line in source.lines() {
        let trimmed = line.trim();
        let inline_module = (trimmed.starts_with("mod ")
            || trimmed.starts_with("pub mod ")
            || (trimmed.starts_with("pub(") && trimmed.contains(" mod ")))
            && trimmed.contains('{');
        if inline_module && previous != "#[cfg(test)]" {
            errors.push(format!(
                "{} uses an unsupported non-test inline module",
                relative_file.display()
            ));
        }
        if !trimmed.is_empty() {
            previous = trimmed;
        }
    }
}

fn attribute_contains_path(tokens: &[Token]) -> bool {
    let mut cursor = 0;
    while cursor + 1 < tokens.len() {
        if tokens[cursor] == Token::Pound && tokens[cursor + 1] == Token::LeftBracket {
            cursor += 2;
            while cursor < tokens.len() && tokens[cursor] != Token::RightBracket {
                if matches!(&tokens[cursor], Token::Ident(value) if value == "path") {
                    return true;
                }
                cursor += 1;
            }
        }
        cursor += 1;
    }
    false
}
