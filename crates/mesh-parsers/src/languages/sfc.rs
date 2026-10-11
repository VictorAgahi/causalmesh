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

/// The byte index of the `>` closing an opening tag that starts at `from`, quotes respected
/// (`<script setup generic="T extends Array<string>">`).
fn tag_end(content: &str, from: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in content[from..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(from + i),
            _ => {}
        }
    }
    None
}

/// The grammar a `<script>` asks for, or `None` for a block that is not JavaScript /
/// TypeScript (`lang="coffee"`, `lang="pug"`, `type="application/ld+json"`, an import map).
fn script_kind(tag: &str) -> Option<LanguageKind> {
    if let Some(ty) = attribute(tag, "type").map(str::to_ascii_lowercase) {
        let js_like = matches!(
            ty.as_str(),
            "" | "module"
                | "text/javascript"
                | "application/javascript"
                | "text/typescript"
                | "application/typescript"
                | "ts"
                | "tsx"
                | "jsx"
        );
        if !js_like {
            return None;
        }
    }
    match attribute(tag, "lang")
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("js" | "javascript" | "jsx" | "mjs") => Some(LanguageKind::JavaScript),
        Some("ts" | "typescript" | "mts") => Some(LanguageKind::TypeScript),
        Some("tsx") => Some(LanguageKind::Tsx),
        Some(_) => None,
    }
}

/// Byte ranges of the content of every `<script …>…</script>` from `from` on, with the grammar
/// each asks for. One pass over the markup: a comment and a `<style>` are skipped whole, every other
/// tag is skipped with its quoted attribute values (`<div :title="'<script>'">` holds no script),
/// and a `<script>` element yields its body. Linear: every position is looked at once.
fn script_blocks(content: &str, from: usize) -> Vec<(usize, usize, LanguageKind)> {
    let lower = content.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = from;
    while let Some(offset) = lower.get(at..).and_then(|rest| rest.find('<')) {
        let open = at + offset;
        let rest = &lower[open..];
        let delimiter_after = |name: &str| {
            rest[name.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '>' || c == '/')
        };
        if rest.starts_with("<!--") {
            at = lower[open + 4..]
                .find("-->")
                .map_or(lower.len(), |i| open + 4 + i + 3);
        } else if rest.starts_with("<style") && delimiter_after("<style") {
            at = lower[open..]
                .find("</style")
                .map_or(lower.len(), |i| open + i + "</style".len());
        } else if rest.starts_with("<script") && delimiter_after("<script") {
            let Some(end) = tag_end(content, open + "<script".len()) else {
                break;
            };
            let tag = &lower[open..end];
            let body_start = end + 1;
            // `<script src="./a.ts" />` has no body: the next block is a different one.
            if tag.trim_end().ends_with('/') {
                at = body_start;
                continue;
            }
            let Some(close) = lower[body_start..].find("</script") else {
                break;
            };
            let body_end = body_start + close;
            if let Some(kind) = script_kind(tag) {
                out.push((body_start, body_end, kind));
            }
            at = body_end + "</script".len();
        } else if rest[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/')
        {
            // Any other tag: skipped with its quoted attribute values. An unterminated
            // quote makes the rest of the file attribute text: nothing more is read.
            match tag_end(content, open + 1) {
                Some(end) => at = end + 1,
                None => break,
            }
        } else {
            at = open + 1;
        }
    }
    out
}

/// The byte range of an Astro component's frontmatter (between the first two `---` lines),
/// which must open the file (a BOM and blank lines before it are allowed).
fn astro_frontmatter(content: &str) -> Option<(usize, usize)> {
    let lead = content.len()
        - content
            .trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
            .len();
    let first_line_end = content[lead..]
        .find('\n')
        .map_or(content.len(), |i| lead + i);
    if content[lead..first_line_end].trim() != "---" {
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
    let mut scan_from = 0;
    if ext == "astro" {
        if let Some((s, e)) = astro_frontmatter(content) {
            ranges.push((s, e, LanguageKind::TypeScript));
            // The client scripts are searched for after the frontmatter closes.
            scan_from = e;
        }
    }
    ranges.extend(script_blocks(content, scan_from));
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
