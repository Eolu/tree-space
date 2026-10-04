//! Lightweight, dependency-free syntax highlighting for inline previews.
//!
//! The preview already caps what it reads (see [`crate::preview`]), so
//! highlighting is a single linear pass over a small buffer. The scanner is
//! deliberately simple — an approximation, not a full parser — and runs once
//! when a document is loaded, caching the resulting spans with it. Rebuilds
//! only replay the cached spans, so there is no per-frame cost.
//!
//! Colors come from a process-wide [`Palette`] set at startup from the active
//! theme (see [`crate::theme`]); the tree turns each [`TokenClass`] into a
//! `GtkTextTag`.

use std::path::Path;
use std::sync::OnceLock;

/// A semantic token category. Each maps to one color in [`Palette`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenClass {
    Keyword,
    Type,
    Function,
    String,
    Comment,
    Number,
    Constant,
    Preprocessor,
    Tag,
    Attribute,
    Heading,
    Link,
}

/// A highlighted range of the preview text, in character offsets (`end` is
/// exclusive). Spans never overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub class: TokenClass,
}

/// The language a preview is highlighted as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    CLike,
    JavaScript,
    TypeScript,
    Python,
    Shell,
    Go,
    Json,
    KeyValue,
    Yaml,
    Markup,
    Css,
    Markdown,
    Plain,
}

/// The colors used for each token class. Values are GTK color strings
/// (`#rrggbb`), so they can be handed straight to a `GtkTextTag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub keyword: String,
    pub type_: String,
    pub function: String,
    pub string: String,
    pub comment: String,
    pub number: String,
    pub constant: String,
    pub preprocessor: String,
    pub tag: String,
    pub attribute: String,
    pub heading: String,
    pub link: String,
}

impl Palette {
    /// The Tokyo Night palette used when no theme is available (and in custom
    /// `main.css` mode, which is dark by default).
    pub fn dark() -> Self {
        Self {
            keyword: "#7aa2f7".to_owned(),
            type_: "#2ac3de".to_owned(),
            function: "#7dcfff".to_owned(),
            string: "#9ece6a".to_owned(),
            comment: "#565f89".to_owned(),
            number: "#ff9e64".to_owned(),
            constant: "#bb9af7".to_owned(),
            preprocessor: "#e0af68".to_owned(),
            tag: "#f7768e".to_owned(),
            attribute: "#bb9af7".to_owned(),
            heading: "#7aa2f7".to_owned(),
            link: "#73daca".to_owned(),
        }
    }

    /// The color for `class`, or `None` when it should use the default text
    /// color.
    pub fn color(&self, class: TokenClass) -> &str {
        match class {
            TokenClass::Keyword => &self.keyword,
            TokenClass::Type => &self.type_,
            TokenClass::Function => &self.function,
            TokenClass::String => &self.string,
            TokenClass::Comment => &self.comment,
            TokenClass::Number => &self.number,
            TokenClass::Constant => &self.constant,
            TokenClass::Preprocessor => &self.preprocessor,
            TokenClass::Tag => &self.tag,
            TokenClass::Attribute => &self.attribute,
            TokenClass::Heading => &self.heading,
            TokenClass::Link => &self.link,
        }
    }
}

/// The palette applied to every preview. Set once during startup via
/// [`set_palette`]; falls back to the dark default if nothing set it.
static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Install the process-wide syntax palette. Only the first call wins.
pub fn set_palette(palette: Palette) {
    let _ = PALETTE.set(palette);
}

/// The active syntax palette.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(Palette::dark)
}

/// Pick the highlighting language from a file's extension.
pub fn language_for(path: &Path) -> Language {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("rs") => Language::Rust,
        Some("c" | "h" | "cc" | "hh" | "cpp" | "hpp" | "cxx" | "hxx") => Language::CLike,
        Some("js" | "mjs" | "cjs" | "jsx") => Language::JavaScript,
        Some("ts" | "mts" | "cts" | "tsx") => Language::TypeScript,
        Some("py" | "pyw") => Language::Python,
        Some("sh" | "bash" | "zsh" | "fish" | "ksh") => Language::Shell,
        Some("go") => Language::Go,
        Some("json") => Language::Json,
        Some("toml" | "ini" | "conf" | "cfg" | "env" | "properties") => Language::KeyValue,
        Some("yaml" | "yml") => Language::Yaml,
        Some("html" | "htm" | "xhtml" | "xml" | "svg") => Language::Markup,
        Some("css") => Language::Css,
        Some("md" | "markdown") => Language::Markdown,
        _ => Language::Plain,
    }
}

/// Highlight `text` as `language`. Character offsets in the returned spans
/// index the same text; the text itself is never altered.
pub fn highlight(language: Language, text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    match language {
        Language::Plain => Vec::new(),
        Language::Rust => scan_code(&chars, &RUST),
        Language::CLike => scan_code(&chars, &C_LIKE),
        Language::JavaScript => scan_code(&chars, &JAVASCRIPT),
        Language::TypeScript => scan_code(&chars, &TYPESCRIPT),
        Language::Python => scan_code(&chars, &PYTHON),
        Language::Shell => scan_code(&chars, &SHELL),
        Language::Go => scan_code(&chars, &GO),
        Language::Json => scan_json(&chars),
        Language::KeyValue => scan_key_value(&chars),
        Language::Yaml => scan_yaml(&chars),
        Language::Markup => scan_markup(&chars),
        Language::Css => scan_css(&chars),
        Language::Markdown => scan_markdown(&chars),
    }
}

// ---------------------------------------------------------------------------
// generic code scanner
// ---------------------------------------------------------------------------

/// Syntax knobs for the generic scanner.
#[derive(Clone, Copy)]
struct Syntax {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// Single-character quote delimiters.
    quotes: &'static [char],
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
    /// `#` at the start of a line begins a preprocessor directive.
    hash_line: bool,
    /// `#[...]` / `#![...]` (Rust attributes) are tagged as one span.
    hash_bracket: bool,
    /// `@name` at the start of a line (Python decorators, shell attributes).
    at_line: bool,
    /// Rust-style raw strings (`r"..."`, `r#"..."#`) and byte strings.
    rust_strings: bool,
    /// Python `"""..."""` / `'''...'''`, and string prefixes.
    python_strings: bool,
}

fn scan_code(chars: &[char], syntax: &Syntax) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // line comments
        if syntax.line_comments.iter().any(|p| matches_at(chars, i, p)) {
            let end = line_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Comment });
            i = end;
            continue;
        }
        // block comments
        if let Some((open, close)) = syntax.block_comment
            && matches_at(chars, i, open)
        {
            let start = i;
            i += char_len(open);
            while i < chars.len() && !matches_at(chars, i, close) {
                i += 1;
            }
            i = (i + char_len(close)).min(chars.len());
            spans.push(Span { start, end: i, class: TokenClass::Comment });
            continue;
        }
        // Rust raw / byte strings
        if syntax.rust_strings {
            if let Some((start, end)) = rust_string_span(chars, i) {
                spans.push(Span { start, end, class: TokenClass::String });
                i = end;
                continue;
            }
            // Rust lifetimes: 'a — not a char literal.
            if c == '\''
                && chars.get(i + 1).is_some_and(|n| is_ident_start(*n))
                && chars.get(i + 2) != Some(&'\'')
            {
                let end = ident_end(chars, i + 1);
                spans.push(Span { start: i, end, class: TokenClass::Type });
                i = end;
                continue;
            }
        }
        // Python triple quotes (also with an optional prefix letter)
        if syntax.python_strings
            && let Some((start, end)) = python_string_span(chars, i)
        {
            spans.push(Span { start, end, class: TokenClass::String });
            i = end;
            continue;
        }
        // plain strings
        if syntax.quotes.contains(&c) {
            let end = string_end(chars, i, c);
            spans.push(Span { start: i, end, class: TokenClass::String });
            i = end;
            continue;
        }
        // Rust attributes
        if syntax.hash_bracket && c == '#' && chars.get(i + 1) == Some(&'[') {
            let end = bracketed_end(chars, i + 1);
            spans.push(Span { start: i, end, class: TokenClass::Preprocessor });
            i = end;
            continue;
        }
        // C preprocessor
        if syntax.hash_line && c == '#' && only_space_before(chars, i) {
            let end = line_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Preprocessor });
            i = end;
            continue;
        }
        // decorators / attributes at line start
        if syntax.at_line && c == '@' && only_space_before(chars, i) {
            let end = ident_end(chars, i + 1);
            spans.push(Span { start: i, end, class: TokenClass::Preprocessor });
            i = end;
            continue;
        }
        // numbers
        if c.is_ascii_digit() && !chars.get(i.wrapping_sub(1)).is_some_and(|p| is_ident_char(*p)) {
            let end = number_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Number });
            i = end;
            continue;
        }
        // identifiers / keywords
        if is_ident_start(c) {
            let end = ident_end(chars, i);
            let word: String = chars[i..end].iter().collect();
            let class = if syntax.constants.contains(&word.as_str()) {
                Some(TokenClass::Constant)
            } else if syntax.keywords.contains(&word.as_str()) {
                Some(TokenClass::Keyword)
            } else if syntax.types.contains(&word.as_str()) {
                Some(TokenClass::Type)
            } else if chars.get(end) == Some(&'(') {
                Some(TokenClass::Function)
            } else {
                None
            };
            if let Some(class) = class {
                spans.push(Span { start: i, end, class });
            }
            i = end;
            continue;
        }
        i += 1;
    }
    spans
}

fn rust_string_span(chars: &[char], i: usize) -> Option<(usize, usize)> {
    let (prefix, quote) = match chars.get(i) {
        Some('r') => (1, true),
        Some('b') if chars.get(i + 1) == Some(&'r') => (2, true),
        Some('b') => (1, false),
        _ => return None,
    };
    if !quote {
        // Byte string b"..."
        if chars.get(i + prefix) == Some(&'"') {
            let start = i;
            let end = string_end(chars, i + prefix, '"');
            return Some((start, end));
        }
        return None;
    }
    let mut j = i + prefix;
    let mut hashes = 0;
    while chars.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if chars.get(j) != Some(&'"') {
        return None;
    }
    j += 1;
    let closing = format!("\"{}", "#".repeat(hashes));
    while j < chars.len() && !matches_at(chars, j, &closing) {
        j += 1;
    }
    let end = (j + char_len(&closing)).min(chars.len());
    Some((i, end))
}

fn python_string_span(chars: &[char], i: usize) -> Option<(usize, usize)> {
    // Optional prefix: r, b, f, u (in any combination), then a quote.
    let mut j = i;
    while chars.get(j).is_some_and(|c| matches!(c, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U')) {
        j += 1;
    }
    let quote = *chars.get(j)?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let triple = chars.get(j + 1) == Some(&quote) && chars.get(j + 2) == Some(&quote);
    let delim = if triple {
        quote.to_string().repeat(3)
    } else {
        quote.to_string()
    };
    let mut k = j + char_len(&delim);
    while k < chars.len() {
        if chars[k] == '\\' {
            k += 2;
            continue;
        }
        if matches_at(chars, k, &delim) {
            return Some((i, k + char_len(&delim)));
        }
        if !triple && chars[k] == '\n' {
            return None;
        }
        k += 1;
    }
    Some((i, chars.len()))
}

/// The end of the string starting at the opening `quote` (inclusive of the
/// closing quote). A backslash escapes the next character.
fn string_end(chars: &[char], start: usize, quote: char) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            c if c == quote => return i + 1,
            '\n' => return i,
            _ => i += 1,
        }
    }
    chars.len()
}

fn number_end(chars: &[char], start: usize) -> usize {
    let mut i = start;
    let mut seen_dot = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '.' {
            // A range operator (`1..2`) is not part of the number.
            if chars.get(i + 1) == Some(&'.') || seen_dot {
                break;
            }
            seen_dot = true;
            i += 1;
        } else if c.is_alphanumeric() || c == '_' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

fn bracketed_end(chars: &[char], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

// ---------------------------------------------------------------------------
// key/value languages
// ---------------------------------------------------------------------------

fn scan_json(chars: &[char]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let end = string_end(chars, i, '"');
            let after = skip_space(chars, end);
            let class = if chars.get(after) == Some(&':') {
                TokenClass::Attribute
            } else {
                TokenClass::String
            };
            spans.push(Span { start: i, end, class });
            i = end;
            continue;
        }
        if c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let end = number_end(chars, i + usize::from(c == '-'));
            spans.push(Span { start: i, end, class: TokenClass::Number });
            i = end;
            continue;
        }
        if is_ident_start(c) {
            let end = ident_end(chars, i);
            let word: String = chars[i..end].iter().collect();
            if matches!(word.as_str(), "true" | "false" | "null") {
                spans.push(Span { start: i, end, class: TokenClass::Constant });
            }
            i = end;
            continue;
        }
        i += 1;
    }
    spans
}

/// TOML / INI / generic `.conf`: `#`/`;` comments, `[section]`, `key = value`.
fn scan_key_value(chars: &[char]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '#' || c == ';' {
            let end = line_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Comment });
            i = end;
            continue;
        }
        if c == '[' && only_space_before(chars, i) {
            let end = chars[i..]
                .iter()
                .position(|&c| c == ']')
                .map(|p| i + p + 1)
                .unwrap_or(chars.len());
            spans.push(Span { start: i, end, class: TokenClass::Tag });
            i = end;
            continue;
        }
        if c == '"' || c == '\'' {
            let end = string_end(chars, i, c);
            // A quoted key (start of line) is an attribute.
            let class = if key_position(chars, i) {
                TokenClass::Attribute
            } else {
                TokenClass::String
            };
            spans.push(Span { start: i, end, class });
            i = end;
            continue;
        }
        if is_ident_start(c) {
            let end = ident_end(chars, i);
            let after = skip_space(chars, end);
            if chars.get(after) == Some(&'=') && key_position(chars, i) {
                spans.push(Span { start: i, end, class: TokenClass::Attribute });
            } else {
                let word: String = chars[i..end].iter().collect();
                if matches!(word.as_str(), "true" | "false") {
                    spans.push(Span { start: i, end, class: TokenClass::Constant });
                }
            }
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let end = number_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Number });
            i = end;
            continue;
        }
        i += 1;
    }
    spans
}

/// YAML: `#` comments, `key:` at the start of a line, quoted strings.
fn scan_yaml(chars: &[char]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '#' {
            let end = line_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Comment });
            i = end;
            continue;
        }
        if c == '"' || c == '\'' {
            let end = string_end(chars, i, c);
            spans.push(Span { start: i, end, class: TokenClass::String });
            i = end;
            continue;
        }
        if is_ident_start(c) && key_position(chars, i) {
            let end = ident_end(chars, i);
            let after = skip_space(chars, end);
            if chars.get(after) == Some(&':') {
                spans.push(Span { start: i, end, class: TokenClass::Attribute });
            }
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let end = number_end(chars, i);
            spans.push(Span { start: i, end, class: TokenClass::Number });
            i = end;
            continue;
        }
        i += 1;
    }
    spans
}

/// True when only whitespace (and YAML list dashes) precede `i` on its line, so
/// the token at `i` is a key rather than a value.
fn key_position(chars: &[char], i: usize) -> bool {
    let mut j = i;
    while j > 0 {
        let c = chars[j - 1];
        if c == '\n' {
            break;
        }
        if !c.is_whitespace() && c != '-' {
            return false;
        }
        j -= 1;
    }
    true
}

// ---------------------------------------------------------------------------
// markup
// ---------------------------------------------------------------------------

fn scan_markup(chars: &[char]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if matches_at(chars, i, "<!--") {
            let start = i;
            i += 4;
            while i < chars.len() && !matches_at(chars, i, "-->") {
                i += 1;
            }
            i = (i + 3).min(chars.len());
            spans.push(Span { start, end: i, class: TokenClass::Comment });
            continue;
        }
        if chars[i] == '<' {
            i = scan_tag(chars, i, &mut spans);
            continue;
        }
        if chars[i] == '&'
            && let Some(end) = chars[i..].iter().position(|&c| c == ';').map(|p| i + p + 1)
            && end - i <= 12
        {
            spans.push(Span { start: i, end, class: TokenClass::Constant });
            i = end;
            continue;
        }
        i += 1;
    }
    spans
}

/// Scan one `<...>` tag starting at `open`, pushing a [`TokenClass::Tag`] span
/// for the name and [`TokenClass::Attribute`] spans for the attributes. Returns
/// the index just past the tag.
fn scan_tag(chars: &[char], open: usize, spans: &mut Vec<Span>) -> usize {
    let mut i = open + 1;
    if chars.get(i) == Some(&'/') {
        i += 1;
    }
    if chars.get(i) == Some(&'!') {
        // doctype: highlight to the closing '>'
        let mut k = i + 1;
        while k < chars.len() && chars[k] != '>' {
            k += 1;
        }
        k = (k + 1).min(chars.len());
        spans.push(Span { start: open, end: k, class: TokenClass::Preprocessor });
        return k;
    }
    let name_start = i;
    while i < chars.len() && is_tag_name_char(chars[i]) {
        i += 1;
    }
    if i > name_start {
        spans.push(Span { start: name_start, end: i, class: TokenClass::Tag });
    }
    while i < chars.len() && chars[i] != '>' {
        if matches_at(chars, i, "/>") {
            return i + 2;
        }
        if chars[i].is_whitespace() || chars[i] == '/' {
            i += 1;
            continue;
        }
        if chars[i] == '"' || chars[i] == '\'' {
            let end = string_end(chars, i, chars[i]);
            spans.push(Span { start: i, end, class: TokenClass::String });
            i = end;
            continue;
        }
        let attr_start = i;
        while i < chars.len() && is_tag_name_char(chars[i]) {
            i += 1;
        }
        if i > attr_start {
            spans.push(Span { start: attr_start, end: i, class: TokenClass::Attribute });
        } else {
            i += 1;
        }
    }
    (i + 1).min(chars.len())
}

fn is_tag_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')
}

// ---------------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------------

fn scan_css(chars: &[char]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = 0;
    let mut in_block = false;
    while i < chars.len() {
        let c = chars[i];
        if matches_at(chars, i, "/*") {
            let start = i;
            i += 2;
            while i < chars.len() && !matches_at(chars, i, "*/") {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            spans.push(Span { start, end: i, class: TokenClass::Comment });
            continue;
        }
        if c == '"' || c == '\'' {
            let end = string_end(chars, i, c);
            spans.push(Span { start: i, end, class: TokenClass::String });
            i = end;
            continue;
        }
        if c == '{' {
            in_block = true;
            i += 1;
            continue;
        }
        if c == '}' {
            in_block = false;
            i += 1;
            continue;
        }
        if c == '@' {
            let end = ident_end(chars, i + 1);
            spans.push(Span { start: i, end, class: TokenClass::Keyword });
            i = end;
            continue;
        }
        if c == '#' && chars.get(i + 1).is_some_and(|d| d.is_ascii_hexdigit()) {
            let mut end = i + 1;
            while end < chars.len() && chars[end].is_ascii_hexdigit() {
                end += 1;
            }
            spans.push(Span { start: i, end, class: TokenClass::Number });
            i = end;
            continue;
        }
        if !in_block {
            if is_ident_start(c) || c == '.' || c == '#' || c == ':' {
                let start = i;
                while i < chars.len()
                    && !matches!(chars[i], '{' | '}' | '\n' | ',' | '(' | ')')
                    && !chars[i].is_whitespace()
                    && !matches_at(chars, i, "/*")
                {
                    i += 1;
                }
                if i > start {
                    spans.push(Span { start, end: i, class: TokenClass::Tag });
                }
                continue;
            }
        } else if is_ident_start(c) {
            let end = ident_end(chars, i);
            let after = skip_space(chars, end);
            if chars.get(after) == Some(&':') {
                spans.push(Span { start: i, end, class: TokenClass::Attribute });
            }
            i = end;
            continue;
        }
        i += 1;
    }
    spans
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn scan_markdown(chars: &[char]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut line_start = 0;
    let mut in_fence = false;
    while line_start < chars.len() {
        let end = line_end(chars, line_start);
        let line = &chars[line_start..end];
        if is_fence(line) {
            spans.push(Span { start: line_start, end, class: TokenClass::Keyword });
            in_fence = !in_fence;
            line_start = end + 1;
            continue;
        }
        if in_fence {
            spans.push(Span { start: line_start, end, class: TokenClass::String });
            line_start = end + 1;
            continue;
        }
        // ATX heading: up to 3 spaces then 1-6 '#'
        let indent = line.iter().take_while(|c| **c == ' ').count();
        if indent <= 3 {
            let hashes = line[indent..].iter().take_while(|c| **c == '#').count();
            if (1..=6).contains(&hashes) && line.get(indent + hashes).is_none_or(|c| *c == ' ') {
                spans.push(Span { start: line_start, end, class: TokenClass::Heading });
                line_start = end + 1;
                continue;
            }
            if line.get(indent) == Some(&'>') {
                spans.push(Span { start: line_start, end, class: TokenClass::Comment });
                line_start = end + 1;
                continue;
            }
        }
        scan_markdown_inline(chars, line_start, end, &mut spans);
        line_start = end + 1;
    }
    spans
}

fn is_fence(line: &[char]) -> bool {
    matches_at(line, 0, "```") || matches_at(line, 0, "~~~")
}

fn scan_markdown_inline(chars: &[char], start: usize, end: usize, spans: &mut Vec<Span>) {
    let mut i = start;
    while i < end {
        if chars[i] == '`'
            && let Some(close) = (i + 1..end).find(|&k| chars[k] == '`')
        {
            spans.push(Span { start: i, end: close + 1, class: TokenClass::String });
            i = close + 1;
            continue;
        }
        if chars[i] == '['
            && let Some(close) = (i + 1..end).find(|&k| chars[k] == ']')
            && chars.get(close + 1) == Some(&'(')
            && let Some(paren) = (close + 2..end).find(|&k| chars[k] == ')')
        {
            spans.push(Span { start: i, end: paren + 1, class: TokenClass::Link });
            i = paren + 1;
            continue;
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Char length of a `&str` (used for delimiters, which are ASCII).
fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn line_end(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

fn skip_space(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// True when every character from the line start to `i` is whitespace.
fn only_space_before(chars: &[char], i: usize) -> bool {
    let mut j = i;
    while j > 0 {
        if chars[j - 1] == '\n' {
            break;
        }
        if !chars[j - 1].is_whitespace() {
            return false;
        }
        j -= 1;
    }
    true
}

fn matches_at(chars: &[char], i: usize, pattern: &str) -> bool {
    for (j, pc) in (i..).zip(pattern.chars()) {
        if chars.get(j) != Some(&pc) {
            return false;
        }
    }
    true
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

fn ident_end(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() && is_ident_char(chars[i]) {
        i += 1;
    }
    i
}

// ---------------------------------------------------------------------------
// language tables
// ---------------------------------------------------------------------------

const RUST: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"'],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "static", "struct", "super", "trait", "type", "unsafe", "use",
        "where", "while", "yield", "macro_rules",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16",
        "u32", "u64", "u128", "usize", "str", "String", "Vec", "Option", "Result", "Box", "Rc",
        "Arc", "RefCell", "HashMap", "HashSet", "BTreeMap", "Cow", "PathBuf", "Path",
    ],
    constants: &["true", "false", "None", "Self"],
    hash_line: false,
    hash_bracket: true,
    at_line: false,
    rust_strings: true,
    python_strings: false,
};

const C_LIKE: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    keywords: &[
        "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern",
        "for", "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static",
        "struct", "switch", "typedef", "union", "volatile", "while", "class", "namespace",
        "template", "typename", "using", "public", "private", "protected", "virtual", "override",
        "new", "delete", "this", "operator", "friend", "constexpr", "try", "catch", "throw",
        "noexcept", "nullptr", "true", "false",
    ],
    types: &[
        "void", "bool", "char", "short", "int", "long", "float", "double", "signed", "unsigned",
        "size_t", "ssize_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t",
        "uint32_t", "uint64_t", "intptr_t", "uintptr_t", "wchar_t", "auto",
    ],
    constants: &["NULL", "nullptr", "true", "false"],
    hash_line: true,
    hash_bracket: false,
    at_line: false,
    rust_strings: false,
    python_strings: false,
};

const JAVASCRIPT: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    keywords: &[
        "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete",
        "do", "else", "export", "extends", "finally", "for", "function", "if", "import", "in",
        "instanceof", "let", "new", "return", "super", "switch", "this", "throw", "try",
        "typeof", "var", "void", "while", "with", "yield", "async", "await", "of", "static",
        "get", "set", "as", "from",
    ],
    types: &["number", "string", "boolean", "object", "symbol", "bigint", "undefined"],
    constants: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    hash_line: false,
    hash_bracket: false,
    at_line: false,
    rust_strings: false,
    python_strings: false,
};

const TYPESCRIPT: Syntax = Syntax {
    keywords: &[
        "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete",
        "do", "else", "export", "extends", "finally", "for", "function", "if", "import", "in",
        "instanceof", "let", "new", "return", "super", "switch", "this", "throw", "try",
        "typeof", "var", "void", "while", "with", "yield", "async", "await", "of", "static",
        "get", "set", "as", "from", "interface", "type", "enum", "namespace", "declare",
        "abstract", "implements", "private", "public", "protected", "readonly", "keyof", "infer",
        "is", "asserts", "satisfies", "override",
    ],
    ..JAVASCRIPT
};

const PYTHON: Syntax = Syntax {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield", "match", "case",
    ],
    types: &["int", "float", "str", "bool", "list", "dict", "set", "tuple", "bytes", "object"],
    constants: &["True", "False", "None", "self", "cls"],
    hash_line: false,
    hash_bracket: false,
    at_line: true,
    rust_strings: false,
    python_strings: true,
};

const SHELL: Syntax = Syntax {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "select", "time", "coproc", "local", "export", "readonly", "declare",
        "unset", "shift", "return", "continue", "break", "source", "alias",
    ],
    types: &["echo", "cd", "pwd", "printf", "read", "set", "test", "trap", "eval", "exec"],
    constants: &["true", "false"],
    hash_line: false,
    hash_bracket: false,
    at_line: false,
    rust_strings: false,
    python_strings: false,
};

const GO: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
        "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
        "return", "select", "struct", "switch", "type", "var",
    ],
    types: &[
        "bool", "string", "int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16",
        "uint32", "uint64", "uintptr", "byte", "rune", "float32", "float64", "complex64",
        "complex128", "error", "any",
    ],
    constants: &["true", "false", "nil", "iota"],
    hash_line: false,
    hash_bracket: false,
    at_line: false,
    rust_strings: false,
    python_strings: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(spans: &[Span]) -> Vec<TokenClass> {
        spans.iter().map(|s| s.class).collect()
    }

    fn text_of(src: &str, spans: &[Span]) -> Vec<String> {
        let chars: Vec<char> = src.chars().collect();
        spans
            .iter()
            .map(|s| chars[s.start..s.end].iter().collect())
            .collect()
    }

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(language_for(Path::new("a.rs")), Language::Rust);
        assert_eq!(language_for(Path::new("a.PY")), Language::Python);
        assert_eq!(language_for(Path::new("a.toml")), Language::KeyValue);
        assert_eq!(language_for(Path::new("a.bin")), Language::Plain);
        assert_eq!(language_for(Path::new("noext")), Language::Plain);
    }

    #[test]
    fn rust_highlights_keywords_strings_comments_attributes() {
        let src = "// hi\n#[derive(Debug)]\nfn main() { let s = \"x\"; let n = 42; }";
        let spans = highlight(Language::Rust, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "// hi"));
        assert!(found.iter().any(|t| t == "#[derive(Debug)]"));
        assert!(found.iter().any(|t| t == "fn"));
        assert!(found.iter().any(|t| t == "\"x\""));
        assert!(found.iter().any(|t| t == "42"));
        assert!(classes(&spans).contains(&TokenClass::Keyword));
        assert!(classes(&spans).contains(&TokenClass::Preprocessor));
    }

    #[test]
    fn python_highlights_decorators_and_triple_strings() {
        let src = "@decorator\ndef f():\n    x = \"\"\"multi\nline\"\"\"\n    return 1";
        let spans = highlight(Language::Python, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "@decorator"));
        assert!(found.iter().any(|t| t.contains("multi")));
        assert!(found.iter().any(|t| t == "def"));
    }

    #[test]
    fn json_distinguishes_keys_from_values() {
        let src = "{ \"name\": \"value\", \"n\": 1 }";
        let spans = highlight(Language::Json, src);
        let key = spans
            .iter()
            .find(|s| s.class == TokenClass::Attribute)
            .map(|s| text_of(src, std::slice::from_ref(s)).remove(0));
        assert_eq!(key.as_deref(), Some("\"name\""));
    }

    #[test]
    fn toml_highlights_sections_and_keys() {
        let src = "[panel]\nwidth = 300\nname = \"tree\"\n";
        let spans = highlight(Language::KeyValue, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "[panel]"));
        assert!(found.iter().any(|t| t == "width"));
        assert!(found.iter().any(|t| t == "300"));
        assert!(found.iter().any(|t| t == "\"tree\""));
    }

    #[test]
    fn yaml_highlights_keys() {
        let src = "name: value\nlist:\n  - one\n";
        let spans = highlight(Language::Yaml, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "name"));
        assert!(found.iter().any(|t| t == "list"));
    }

    #[test]
    fn markup_highlights_tags_and_attributes() {
        let src = "<a href=\"x\">text</a>";
        let spans = highlight(Language::Markup, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "a"));
        assert!(found.iter().any(|t| t == "href"));
        assert!(found.iter().any(|t| t == "\"x\""));
    }

    #[test]
    fn css_highlights_at_rules_and_properties() {
        let src = "/* c */\n.foo { color: #fff; }";
        let spans = highlight(Language::Css, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "/* c */"));
        assert!(found.iter().any(|t| t == "color"));
        assert!(found.iter().any(|t| t == "#fff"));
    }

    #[test]
    fn markdown_highlights_headings_code_and_links() {
        let src = "# Title\n\nSee [docs](http://x) and `code`.\n";
        let spans = highlight(Language::Markdown, src);
        let found = text_of(src, &spans);
        assert!(found.iter().any(|t| t == "# Title"));
        assert!(found.iter().any(|t| t == "[docs](http://x)"));
        assert!(found.iter().any(|t| t == "`code`"));
    }

    #[test]
    fn spans_are_sorted_bounded_and_non_overlapping() {
        let src = "fn f() { // c\n    \"s\" 1 }";
        let spans = highlight(Language::CLike, src);
        let mut last = 0;
        for span in &spans {
            assert!(span.start < span.end, "empty span: {span:?}");
            assert!(span.start >= last, "overlap/unsorted at {span:?}");
            last = span.end;
        }
        assert!(spans.iter().all(|s| s.end <= src.chars().count()));
    }

    #[test]
    fn plain_language_has_no_spans() {
        assert!(highlight(Language::Plain, "anything at all").is_empty());
    }
}
