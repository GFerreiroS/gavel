//! Minimal `.env` loading.
//!
//! Secrets come from the environment (CLAUDE.md 28/30), but "the environment"
//! in development means a `.env` file, and nothing was reading one -- so
//! credentials sat on disk while the app reported them missing.
//!
//! Hand-rolled rather than pulling in a crate: the format is a dozen lines of
//! parsing, and the parser is a pure function so it can be tested without
//! touching real process state.
//!
//! Two deliberate behaviours:
//!
//! * **A real environment variable always wins.** A `.env` is a development
//!   convenience; it must never silently override what a deployment sets.
//! * **Values are never logged.** Only key names are reported, and only to say
//!   which ones were found.

use std::path::{Path, PathBuf};

/// One parsed assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

/// Parse `.env` contents. Pure: no process state is touched.
pub fn parse(contents: &str) -> Vec<Entry> {
    contents.lines().filter_map(parse_line).collect()
}

fn parse_line(raw: &str) -> Option<Entry> {
    // Tolerate CRLF: a stray '\r' welded onto a secret produces an
    // authentication failure with no visible cause.
    let line = raw.trim_end_matches('\r').trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, value) = line.split_once('=')?;

    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }

    Some(Entry {
        key: key.to_string(),
        value: clean_value(value.trim()),
    })
}

/// Strip surrounding quotes, or an unquoted trailing comment.
fn clean_value(value: &str) -> String {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            // Quoted: everything inside is literal, '#' included.
            return value[1..value.len() - 1].to_string();
        }
    }
    // Unquoted: a ' #' begins a comment. Requires the space so that a '#'
    // inside a URL fragment or a secret survives.
    match value.split_once(" #") {
        Some((before, _)) => before.trim_end().to_string(),
        None => value.to_string(),
    }
}

/// Find a `.env` by walking up from the working directory.
///
/// Walking up means `cargo run -p server` from a subdirectory still finds the
/// file at the repository root.
pub fn find(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    for _ in 0..4 {
        let candidate = dir?.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir?.parent();
    }
    None
}

/// Load `.env` into the process environment, returning the keys applied.
///
/// # Safety contract
///
/// `std::env::set_var` is not thread-safe. This must be called from `main`
/// before any threads exist -- in particular before the Tokio runtime starts.
pub fn load_from(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let mut applied = Vec::new();
    for entry in parse(&contents) {
        // A real environment variable wins: the file is a dev convenience.
        if std::env::var_os(&entry.key).is_some() {
            continue;
        }
        // SAFETY: called from `main` before the runtime is built, so no other
        // thread can be reading the environment concurrently.
        unsafe { std::env::set_var(&entry.key, &entry.value) };
        applied.push(entry.key);
    }
    applied
}

/// Convenience: find and load, reporting what happened.
///
/// Returns `(path, keys)`; an empty path means no file was found.
pub fn load_default() -> (Option<PathBuf>, Vec<String>) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match find(&cwd) {
        Some(path) => {
            let keys = load_from(&path);
            (Some(path), keys)
        }
        None => (None, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str) -> Vec<(String, String)> {
        parse(input).into_iter().map(|e| (e.key, e.value)).collect()
    }

    #[test]
    fn parses_plain_assignments() {
        assert_eq!(
            parsed("A=1\nB=two"),
            vec![("A".into(), "1".into()), ("B".into(), "two".into())]
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        assert_eq!(
            parsed("# a comment\n\n   \nA=1\n"),
            vec![("A".into(), "1".into())]
        );
    }

    #[test]
    fn accepts_an_export_prefix() {
        assert_eq!(parsed("export A=1"), vec![("A".into(), "1".into())]);
    }

    #[test]
    fn strips_surrounding_quotes_but_keeps_the_contents() {
        assert_eq!(parsed(r#"A="a b""#), vec![("A".into(), "a b".into())]);
        assert_eq!(parsed("A='a b'"), vec![("A".into(), "a b".into())]);
        // A '#' inside quotes is part of the value, not a comment.
        assert_eq!(parsed(r##"A="v#1""##), vec![("A".into(), "v#1".into())]);
    }

    #[test]
    fn a_value_may_contain_equals_signs() {
        // Base64 secrets and webhook URLs routinely do.
        assert_eq!(
            parsed("A=abc==def?x=1"),
            vec![("A".into(), "abc==def?x=1".into())]
        );
    }

    #[test]
    fn crlf_endings_do_not_corrupt_the_value() {
        // The failure this guards against is silent: a trailing '\r' welded to
        // a client secret just produces a 401.
        assert_eq!(
            parsed("A=secret\r\nB=2\r\n"),
            vec![("A".into(), "secret".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn trailing_comments_are_dropped_only_when_unquoted() {
        assert_eq!(parsed("A=value # note"), vec![("A".into(), "value".into())]);
        // No space before '#': part of the value, because URLs contain them.
        assert_eq!(parsed("A=val#ue"), vec![("A".into(), "val#ue".into())]);
    }

    #[test]
    fn empty_values_are_kept_as_empty() {
        // The catalog of optional credentials in .env.example ships like this.
        assert_eq!(parsed("A="), vec![("A".into(), "".into())]);
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        assert_eq!(
            parsed("no equals sign\n=novalue\nA B=1\nGOOD=1"),
            vec![("GOOD".into(), "1".into())]
        );
    }
}
