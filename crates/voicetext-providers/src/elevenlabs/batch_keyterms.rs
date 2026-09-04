use std::collections::HashSet;

pub(super) fn canonicalize_keyterms(input: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    for term in input {
        let normalized = term.split_whitespace().collect::<Vec<_>>().join(" ");
        if seen.insert(normalized.clone()) {
            output.push(normalized);
        }
    }
    output.sort();
    output
}
