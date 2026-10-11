//! Single-file components (`.vue`, `.svelte`, `.astro`): the code of the component is its
//! `<script>` blocks (and, for Astro, the `---` frontmatter), parsed by the JavaScript /
//! TypeScript extractor.
//!
//! Everything outside those blocks is blanked, byte for byte, newlines kept: the script
//! then parses on its own and every line number and column it reports is the one of the
//! original file.

use crate::decapitate::LanguageKind;
use std::path::Path;

/// The script code of a single-file component and the grammar to read it with.
pub(crate) struct ScriptView {
    /// The file with everything but its script blocks blanked (same length, same lines).
    pub script: String,
    pub kind: LanguageKind,
}

fn extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|e| e.to_str())
}

/// Whether `path` is a single-file component this module reads.
pub(crate) fn is_sfc(path: &Path) -> bool {
    matches!(extension(path), Some("vue" | "svelte" | "astro"))
}

/// The value of attribute `name` in the text of an opening tag (`lang="ts"`, `lang='ts'`,
/// `lang=ts`).
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(at) = rest.find(name) {
        let before_ok = at == 0 || !rest[..at].ends_with(|c: char| c.is_alphanumeric() || c == '-');
        let after = &rest[at + name.len()..];
        let after = after.trim_start();
        if before_ok {
            if let Some(value) = after.strip_prefix('=') {
                let value = value.trim_start();
                return match value.chars().next() {
                    Some(q @ ('"' | '\'')) => value[1..].split(q).next(),
                    _ => value.split(|c: char| c.is_whitespace() || c == '>').next(),
                };
            }
        }
        rest = &rest[at + name.len()..];
    }
    None
}

/// Byte ranges of the content of every `<script …>…</script>` and the grammar each asks for.
fn script_blocks(content: &str) -> Vec<(usize, usize, LanguageKind)> {
    let lower = content.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = lower[from..].find("<script") {
        let open = from + open;
        let after_name = open + "<script".len();
        // `<scripts>` or `<script-foo>` is not a script element.
        if !lower[after_name..].starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            from = after_name;
            continue;
        }
        let Some(tag_end) = lower[after_name..].find('>') else {
            break;
        };
        let tag_end = after_name + tag_end;
        let tag = &content[open..tag_end];
        let body_start = tag_end + 1;
        let self_closing = tag.ends_with('/');
        let Some(close) = lower[body_start..].find("</script") else {
            break;
        };
        let body_end = body_start + close;
        let lang = attribute(tag, "lang").map(str::to_ascii_lowercase);
        let kind = match lang.as_deref() {
            Some("tsx") => LanguageKind::Tsx,
            Some("ts" | "typescript") => LanguageKind::TypeScript,
            _ => LanguageKind::JavaScript,
        };
        if !self_closing {
            out.push((body_start, body_end, kind));
        }
        from = body_end;
    }
    out
}

/// The byte range of an Astro component's frontmatter (between the first two `---` lines).
fn astro_frontmatter(content: &str) -> Option<(usize, usize)> {
    let first_line_end = content.find('\n').unwrap_or(content.len());
    if content[..first_line_end].trim() != "---" {
        return None;
    }
    let start = (first_line_end + 1).min(content.len());
    let mut offset = start;
    for line in content[start..].split_inclusive('\n') {
        if line.trim() == "---" {
            return Some((start, offset));
        }
        offset += line.len();
    }
    None
}

/// The script view of a single-file component; `None` for any other file, or a
/// component with no script.
pub(crate) fn script_view(path: &Path, content: &str) -> Option<ScriptView> {
    let ext = extension(path)?;
    if !matches!(ext, "vue" | "svelte" | "astro") {
        return None;
    }
    let mut ranges: Vec<(usize, usize, LanguageKind)> = Vec::new();
    if ext == "astro" {
        if let Some((s, e)) = astro_frontmatter(content) {
            ranges.push((s, e, LanguageKind::TypeScript));
        }
    }
    ranges.extend(script_blocks(content));
    if ranges.is_empty() {
        return None;
    }
    // A TypeScript block makes the component TypeScript; `lang="tsx"` makes it TSX.
    let kind = if ranges.iter().any(|r| r.2 == LanguageKind::Tsx) {
        LanguageKind::Tsx
    } else if ranges.iter().any(|r| r.2 == LanguageKind::TypeScript) {
        LanguageKind::TypeScript
    } else {
        LanguageKind::JavaScript
    };
    let mut bytes = content.as_bytes().to_vec();
    let mut cursor = 0;
    ranges.sort_by_key(|r| r.0);
    for (start, end, _) in &ranges {
        for b in &mut bytes[cursor..(*start).max(cursor)] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
        cursor = *end;
    }
    for b in &mut bytes[cursor..] {
        if *b != b'\n' && *b != b'\r' {
            *b = b' ';
        }
    }
    // Only ASCII bytes were written over whole characters, so this stays valid UTF-8.
    let script = String::from_utf8(bytes).ok()?;
    Some(ScriptView { script, kind })
}
