//! Valve KeyValues parsing: text VDF (PICS app info) and binary KV
//! (package info, machine ids).

use anyhow::{anyhow, bail, Context};

#[derive(Debug, Clone, Default)]
pub struct KeyValue {
    pub name: String,
    pub value: Option<String>,
    pub children: Vec<KeyValue>,
}

impl KeyValue {
    pub fn new(name: impl Into<String>) -> Self {
        KeyValue { name: name.into(), value: None, children: Vec::new() }
    }

    pub fn with_value(name: impl Into<String>, value: impl Into<String>) -> Self {
        KeyValue { name: name.into(), value: Some(value.into()), children: Vec::new() }
    }

    /// Finds the first direct child with the given name.
    pub fn get(&self, name: &str) -> Option<&KeyValue> {
        self.children.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Navigates a path like "depots/731/manifests/public/gid".
    pub fn path(&self, path: &str) -> Option<&KeyValue> {
        let mut cur = self;
        for part in path.split('/') {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    pub fn value_str(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.value.as_deref()?.trim().parse().ok()
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.value.as_deref()?.trim().parse().ok()
    }

    pub fn as_bool(&self) -> bool {
        matches!(self.value.as_deref(), Some("1") | Some("true") | Some("True"))
    }
}

// ============================================================
// Text VDF parsing
// ============================================================

pub fn parse_text_vdf(data: &[u8]) -> anyhow::Result<KeyValue> {
    let text = String::from_utf8_lossy(data);
    let tokens = tokenize(&text)?;
    let mut pos = 0usize;

    // Top level: one or more root nodes.
    let mut root = KeyValue::new("");
    while pos < tokens.len() {
        let name = tokens[pos].clone();
        pos += 1;
        if pos >= tokens.len() {
            bail!("unexpected EOF after key '{}'", name);
        }
        if tokens[pos] == "{" {
            pos += 1;
            let node = parse_block(&tokens, &mut pos, name)?;
            root.children.push(node);
        } else {
            // value node
            root.children.push(KeyValue::with_value(name, tokens[pos].clone()));
            pos += 1;
            // skip optional platform conditional like [~$WIN32]
            if pos < tokens.len() && tokens[pos].starts_with('[') {
                pos += 1;
            }
        }
    }
    Ok(root)
}

fn parse_block(tokens: &[String], pos: &mut usize, name: String) -> anyhow::Result<KeyValue> {
    let mut node = KeyValue::new(name);
    loop {
        if *pos >= tokens.len() {
            bail!("unexpected EOF inside block '{}'", node.name);
        }
        let tok = &tokens[*pos];
        if tok == "}" {
            *pos += 1;
            return Ok(node);
        }
        let key = tok.clone();
        *pos += 1;
        if *pos >= tokens.len() {
            bail!("unexpected EOF after key '{}'", key);
        }
        if tokens[*pos] == "{" {
            *pos += 1;
            let child = parse_block(tokens, pos, key)?;
            node.children.push(child);
        } else {
            let value = tokens[*pos].clone();
            *pos += 1;
            node.children.push(KeyValue::with_value(key, value));
        }
        // skip optional platform conditional
        if *pos < tokens.len() && tokens[*pos].starts_with('[') {
            *pos += 1;
        }
    }
}

/// Tokenizes VDF text into strings / braces, honoring quotes, escapes and
/// `//` comments. Conditional segments `[...]` are emitted as single tokens.
fn tokenize(text: &str) -> anyhow::Result<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut tokens = Vec::new();

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                i += 1;
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '{' | '}' => {
                tokens.push(c.to_string());
                i += 1;
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(anyhow!("unterminated quoted string"));
                    }
                    let ch = chars[i];
                    if ch == '\\' && i + 1 < chars.len() {
                        let next = chars[i + 1];
                        let unescaped = match next {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '\\' => '\\',
                            '"' => '"',
                            other => other,
                        };
                        s.push(unescaped);
                        i += 2;
                        continue;
                    }
                    if ch == '"' {
                        i += 1;
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                tokens.push(s);
            }
            '[' => {
                // platform conditional, consume to ']'
                let mut s = String::new();
                while i < chars.len() && chars[i] != ']' {
                    s.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                tokens.push(s);
            }
            _ => {
                // bare token (unquoted)
                let mut s = String::new();
                while i < chars.len() {
                    let ch = chars[i];
                    if ch.is_whitespace() || ch == '{' || ch == '}' || ch == '"' {
                        break;
                    }
                    if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                if s.is_empty() {
                    i += 1; // safety: avoid infinite loop
                } else {
                    tokens.push(s);
                }
            }
        }
    }
    Ok(tokens)
}

// ============================================================
// Binary KeyValues parsing
// ============================================================

const TYPE_NONE: u8 = 0;
const TYPE_STRING: u8 = 1;
const TYPE_INT32: u8 = 2;
const TYPE_FLOAT32: u8 = 3;
const TYPE_POINTER: u8 = 4;
const TYPE_WIDESTRING: u8 = 5;
const TYPE_COLOR: u8 = 6;
const TYPE_UINT64: u8 = 7;
const TYPE_END: u8 = 8;
const TYPE_INT64: u8 = 10;
const TYPE_ALTERNATE_END: u8 = 11;

struct BinReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BinReader<'a> {
    fn u8(&mut self) -> anyhow::Result<u8> {
        if self.pos >= self.data.len() {
            bail!("binary KV: unexpected EOF");
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("binary KV: unexpected EOF (need {})", n);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes(s.try_into().unwrap()))
    }

    fn u64(&mut self) -> anyhow::Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes(s.try_into().unwrap()))
    }

    fn i64(&mut self) -> anyhow::Result<i64> {
        let s = self.take(8)?;
        Ok(i64::from_le_bytes(s.try_into().unwrap()))
    }

    fn f32(&mut self) -> anyhow::Result<f32> {
        let s = self.take(4)?;
        Ok(f32::from_le_bytes(s.try_into().unwrap()))
    }

    fn cstr(&mut self) -> anyhow::Result<String> {
        let start = self.pos;
        while self.pos < self.data.len() && self.data[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.data.len() {
            bail!("binary KV: unterminated string");
        }
        let s = String::from_utf8_lossy(&self.data[start..self.pos]).into_owned();
        self.pos += 1; // skip NUL
        Ok(s)
    }
}

/// Parses binary KeyValues (as found in package info buffers).
pub fn parse_binary_kv(data: &[u8]) -> anyhow::Result<KeyValue> {
    let mut r = BinReader { data, pos: 0 };
    let mut root = KeyValue::new("");
    parse_binary_node(&mut r, &mut root).context("binary KV parse")?;
    Ok(root)
}

fn parse_binary_node(r: &mut BinReader, parent: &mut KeyValue) -> anyhow::Result<()> {
    loop {
        let ty = r.u8()?;
        if ty == TYPE_END || ty == TYPE_ALTERNATE_END {
            return Ok(());
        }
        let name = r.cstr()?;
        match ty {
            TYPE_NONE => {
                let mut child = KeyValue::new(name);
                parse_binary_node(r, &mut child)?;
                parent.children.push(child);
            }
            TYPE_STRING => {
                let value = r.cstr()?;
                parent.children.push(KeyValue::with_value(name, value));
            }
            TYPE_WIDESTRING => {
                // UTF-16LE NUL-terminated; rare in practice, decode best-effort
                let start = r.pos;
                loop {
                    if r.pos + 1 >= r.data.len() {
                        bail!("binary KV: unterminated widestring");
                    }
                    if r.data[r.pos] == 0 && r.data[r.pos + 1] == 0 {
                        break;
                    }
                    r.pos += 2;
                }
                let mut u16s = Vec::new();
                let mut i = start;
                while i + 1 < r.pos {
                    u16s.push(u16::from_le_bytes([r.data[i], r.data[i + 1]]));
                    i += 2;
                }
                r.pos += 2;
                let value = String::from_utf16_lossy(&u16s);
                parent.children.push(KeyValue::with_value(name, value));
            }
            TYPE_INT32 | TYPE_COLOR | TYPE_POINTER => {
                let v = r.u32()?;
                parent.children.push(KeyValue::with_value(name, v.to_string()));
            }
            TYPE_UINT64 => {
                let v = r.u64()?;
                parent.children.push(KeyValue::with_value(name, v.to_string()));
            }
            TYPE_INT64 => {
                let v = r.i64()?;
                parent.children.push(KeyValue::with_value(name, v.to_string()));
            }
            TYPE_FLOAT32 => {
                let v = r.f32()?;
                parent.children.push(KeyValue::with_value(name, v.to_string()));
            }
            other => bail!("binary KV: unknown type {}", other),
        }
    }
}
