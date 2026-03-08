//! Key syntax parser for `tv exec`.
//!
//! Parses CLI arguments into sequences of byte groups that can be written to a PTY.
//!
//! # Syntax
//!
//! Each CLI argument is either:
//! - **Literal text**: plain characters sent as UTF-8 bytes (e.g. `"ls -la"`)
//! - **Key expression**: `{key_spec}` where key_spec is a key name with optional modifiers
//! - **Sleep**: `{sleep:DURATION}` pauses for a fixed duration
//! - **Wait**: `{wait:DURATION}` waits for output to settle (or `{wait:D:T}` with custom idle threshold)
//! - **Escaped braces**: `{{` and `}}` produce literal `{` and `}` characters
//!
//! # Key Expressions
//!
//! ```text
//! {cr}            → Enter (\r)
//! {esc}           → Escape (\x1b)
//! {c-c}           → Ctrl+C (\x03)
//! {sleep:2s}      → 2 second hard pause
//! {sleep:500ms}   → 500ms hard pause
//! {wait:5s}       → wait up to 5s for output to settle
//! {wait:30s:2s}   → wait up to 30s, with 2s idle threshold
//! ```
//!
//! # Duration Syntax
//!
//! Durations support units: `ms`, `s`, `m`, `h`. Compound durations like `1m30s`
//! are supported. Bare numbers are treated as milliseconds for backward compatibility.
//!
//! # Modifiers
//!
//! - `c-` Ctrl
//! - `s-` Shift
//! - `a-` Alt/Meta
//!
//! Modifiers can be combined: `{c-s-up}` for Ctrl+Shift+Up.
//!
//! # Modifier Encoding (xterm)
//!
//! For arrow/special keys with modifiers, the CSI parameter is:
//! Shift=2, Alt=3, Shift+Alt=4, Ctrl=5, Ctrl+Shift=6, Ctrl+Alt=7, Ctrl+Shift+Alt=8

use std::time::Duration;

/// A parsed group of key data.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyGroup {
    /// Raw bytes to write to the PTY.
    Bytes(Vec<u8>),
    /// Hard pause — always waits exactly N ms.
    Sleep(Duration),
    /// Soft wait — waits up to `timeout` for PTY output to go idle.
    /// Responds early if no output for `idle_threshold` (or config default if None).
    /// Falls back to timeout if output never settles.
    Wait {
        timeout: Duration,
        idle_threshold: Option<Duration>,
    },
    /// Placeholder for piped stdin content. Resolved to Bytes before execution.
    Stdin,
}

/// Parse all CLI exec arguments into a list of key groups.
///
/// Each argument is parsed independently with internal merging (so `"ls{cr}"`
/// becomes a single `Bytes` group). Boundaries between arguments are preserved
/// so that inter-argument delays can be inserted by the caller.
pub fn parse_exec_args(args: &[String]) -> Result<Vec<KeyGroup>, String> {
    let mut groups = Vec::new();

    for arg in args {
        let parsed = parse_arg(arg)?;
        groups.extend(parsed);
    }

    Ok(groups)
}

/// Parse a single CLI argument into key groups.
///
/// An argument can contain a mix of literal text and key expressions:
/// - `"hello{cr}"` → [Bytes(b"hello\r")]  (merged within one arg)
/// - `"{c-c}"` → [Bytes(b"\x03")]
/// - `"{sleep:500}"` → [Sleep(500ms)]
/// - `"{{cr}}"` → [Bytes(b"{cr}")]
fn parse_arg(arg: &str) -> Result<Vec<KeyGroup>, String> {
    let mut groups = Vec::new();
    let mut chars = arg.chars().peekable();
    let mut literal = String::new();

    while let Some(&ch) = chars.peek() {
        match ch {
            '{' => {
                chars.next();
                if chars.peek() == Some(&'{') {
                    // Escaped opening brace: {{ → literal {
                    chars.next();
                    literal.push('{');
                } else {
                    // Start of key expression — flush any accumulated literal
                    if !literal.is_empty() {
                        groups.push(KeyGroup::Bytes(literal.as_bytes().to_vec()));
                        literal.clear();
                    }

                    // Collect until closing }
                    let mut expr = String::new();
                    let mut found_close = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            found_close = true;
                            break;
                        }
                        expr.push(c);
                    }

                    if !found_close {
                        return Err(format!("unclosed key expression: {{{}", expr));
                    }

                    if expr.is_empty() {
                        return Err("empty key expression: {}".to_string());
                    }

                    groups.push(parse_key_expression(&expr)?);
                }
            }
            '}' => {
                chars.next();
                if chars.peek() == Some(&'}') {
                    // Escaped closing brace: }} → literal }
                    chars.next();
                    literal.push('}');
                } else {
                    // Stray } without matching {
                    return Err("unexpected '}' without matching '{'".to_string());
                }
            }
            _ => {
                chars.next();
                literal.push(ch);
            }
        }
    }

    // Flush remaining literal
    if !literal.is_empty() {
        groups.push(KeyGroup::Bytes(literal.as_bytes().to_vec()));
    }

    // Merge adjacent Bytes within this single arg
    merge_bytes(&mut groups);

    Ok(groups)
}

/// Parse a duration string with support for units and compound durations.
///
/// Supported formats:
/// - Bare number: `500` → 500ms (backward compatible)
/// - With unit: `500ms`, `2s`, `1.5s`, `1m`, `1h`
/// - Compound: `1m30s`, `1h10m`, `2m500ms`
///
/// Units: `ms` (milliseconds), `s` (seconds), `m` (minutes), `h` (hours)
fn parse_duration(s: &str) -> Result<Duration, String> {
    if s.is_empty() {
        return Err("empty duration".to_string());
    }

    let mut remaining = s;
    let mut total_ms: f64 = 0.0;
    let mut has_units = false;

    while !remaining.is_empty() {
        // Parse numeric part (integer or decimal)
        let num_end = remaining
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(remaining.len());

        if num_end == 0 {
            return Err(format!("invalid duration: '{}'", s));
        }

        let num: f64 = remaining[..num_end]
            .parse()
            .map_err(|_| format!("invalid duration: '{}'", s))?;

        remaining = &remaining[num_end..];

        // Parse unit suffix
        let multiplier = if remaining.starts_with("ms") {
            remaining = &remaining[2..];
            has_units = true;
            1.0
        } else if remaining.starts_with("h") {
            remaining = &remaining[1..];
            has_units = true;
            3_600_000.0
        } else if remaining.starts_with("m") {
            remaining = &remaining[1..];
            has_units = true;
            60_000.0
        } else if remaining.starts_with("s") {
            remaining = &remaining[1..];
            has_units = true;
            1_000.0
        } else if remaining.is_empty() && !has_units {
            // Bare number with no suffix — treat as ms (backward compat)
            1.0
        } else {
            return Err(format!("invalid duration: '{}'", s));
        };

        total_ms += num * multiplier;
    }

    Ok(Duration::from_millis(total_ms.round() as u64))
}

/// Parse a key expression (the part inside `{...}`) into a KeyGroup.
fn parse_key_expression(expr: &str) -> Result<KeyGroup, String> {
    // Stdin placeholder — resolved to Bytes by the session with piped content
    if expr == "stdin" {
        return Ok(KeyGroup::Stdin);
    }

    // Detect bare {sleep} and {wait} without duration — give targeted error
    if expr == "sleep" {
        return Err("sleep requires a duration, e.g. {sleep:500ms}".to_string());
    }
    if expr == "wait" {
        return Err(
            "wait requires a duration and is used to wait for terminal output to settle, e.g. {wait:5s}"
                .to_string(),
        );
    }

    // Check for sleep: {sleep:DURATION}
    if let Some(dur_str) = expr.strip_prefix("sleep:") {
        let duration = parse_duration(dur_str).map_err(|e| format!("invalid sleep: {}", e))?;
        return Ok(KeyGroup::Sleep(duration));
    }

    // Check for wait: {wait:DURATION} or {wait:DURATION:THRESHOLD}
    if let Some(rest) = expr.strip_prefix("wait:") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        return match parts.len() {
            1 => {
                let timeout =
                    parse_duration(parts[0]).map_err(|e| format!("invalid wait timeout: {}", e))?;
                Ok(KeyGroup::Wait {
                    timeout,
                    idle_threshold: None,
                })
            }
            2 => {
                let timeout =
                    parse_duration(parts[0]).map_err(|e| format!("invalid wait timeout: {}", e))?;
                let threshold = parse_duration(parts[1])
                    .map_err(|e| format!("invalid idle threshold: {}", e))?;
                Ok(KeyGroup::Wait {
                    timeout,
                    idle_threshold: Some(threshold),
                })
            }
            _ => unreachable!("splitn(2) returns at most 2 parts"),
        };
    }

    // Parse modifiers
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut remaining = expr;

    loop {
        if let Some(rest) = remaining.strip_prefix("c-") {
            if ctrl {
                return Err(format!("duplicate ctrl modifier in: {}", expr));
            }
            ctrl = true;
            remaining = rest;
        } else if let Some(rest) = remaining.strip_prefix("s-") {
            if shift {
                return Err(format!("duplicate shift modifier in: {}", expr));
            }
            shift = true;
            remaining = rest;
        } else if let Some(rest) = remaining.strip_prefix("a-") {
            if alt {
                return Err(format!("duplicate alt modifier in: {}", expr));
            }
            alt = true;
            remaining = rest;
        } else {
            break;
        }
    }

    if remaining.is_empty() {
        return Err(format!("missing key name in: {}", expr));
    }

    let bytes = encode_key(remaining, ctrl, shift, alt)?;
    Ok(KeyGroup::Bytes(bytes))
}

/// Encode a key name with modifiers into terminal bytes.
fn encode_key(name: &str, ctrl: bool, shift: bool, alt: bool) -> Result<Vec<u8>, String> {
    // Single character keys (for ctrl/alt combos like {c-a}, {a-x})
    if name.len() == 1 {
        let ch = name.chars().next().expect("non-empty after len check");
        return encode_char_key(ch, ctrl, shift, alt);
    }

    // Named keys
    match name {
        // Simple keys
        "cr" | "enter" => encode_simple_key(b"\r", ctrl, shift, alt),
        "esc" => Ok(vec![0x1b]),
        "tab" => {
            if shift && !ctrl && !alt {
                // Shift+Tab = backtab
                Ok(vec![0x1b, b'[', b'Z'])
            } else if ctrl || alt {
                encode_simple_key(b"\t", ctrl, shift, alt)
            } else {
                Ok(vec![b'\t'])
            }
        }
        "bs" | "backspace" => encode_simple_key(&[0x7f], ctrl, shift, alt),
        "space" => encode_simple_key(b" ", ctrl, shift, alt),
        "nul" => Ok(vec![0x00]),
        "del" | "delete" => encode_csi_key(b"3~", ctrl, shift, alt),

        // Arrow keys
        "up" => encode_csi_key(b"A", ctrl, shift, alt),
        "down" => encode_csi_key(b"B", ctrl, shift, alt),
        "right" => encode_csi_key(b"C", ctrl, shift, alt),
        "left" => encode_csi_key(b"D", ctrl, shift, alt),

        // Navigation
        "home" => encode_csi_key(b"H", ctrl, shift, alt),
        "end" => encode_csi_key(b"F", ctrl, shift, alt),
        "insert" => encode_csi_key(b"2~", ctrl, shift, alt),
        "pageup" => encode_csi_key(b"5~", ctrl, shift, alt),
        "pagedown" => encode_csi_key(b"6~", ctrl, shift, alt),

        // Function keys
        "f1" => encode_ss3_or_csi(b"P", b"P", ctrl, shift, alt),
        "f2" => encode_ss3_or_csi(b"Q", b"Q", ctrl, shift, alt),
        "f3" => encode_ss3_or_csi(b"R", b"R", ctrl, shift, alt),
        "f4" => encode_ss3_or_csi(b"S", b"S", ctrl, shift, alt),
        "f5" => encode_csi_tilde(15, ctrl, shift, alt),
        "f6" => encode_csi_tilde(17, ctrl, shift, alt),
        "f7" => encode_csi_tilde(18, ctrl, shift, alt),
        "f8" => encode_csi_tilde(19, ctrl, shift, alt),
        "f9" => encode_csi_tilde(20, ctrl, shift, alt),
        "f10" => encode_csi_tilde(21, ctrl, shift, alt),
        "f11" => encode_csi_tilde(23, ctrl, shift, alt),
        "f12" => encode_csi_tilde(24, ctrl, shift, alt),

        _ => Err(format!("unknown key name: '{}'", name)),
    }
}

/// Encode a single character key with modifiers.
fn encode_char_key(ch: char, ctrl: bool, shift: bool, alt: bool) -> Result<Vec<u8>, String> {
    if !ch.is_ascii() {
        return Err(format!(
            "non-ASCII character '{}' not supported in key expressions",
            ch
        ));
    }

    let mut bytes = Vec::new();

    if ctrl && ch.is_ascii_alphabetic() {
        // Ctrl+letter: a-z → 0x01-0x1a
        let base = ch.to_ascii_lowercase() as u8;
        let ctrl_byte = base - b'a' + 1;
        if alt {
            bytes.push(0x1b);
        }
        bytes.push(ctrl_byte);
        // shift is ignored for ctrl+letter (ctrl already changes the byte)
        return Ok(bytes);
    }

    if ctrl {
        // Ctrl + special chars
        let ctrl_byte = match ch {
            '[' => 0x1b,  // ESC
            '\\' => 0x1c, // SIGQUIT
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '@' => 0x00,
            _ => {
                return Err(format!(
                    "ctrl+{} is not a valid control character combination",
                    ch
                ))
            }
        };
        if alt {
            bytes.push(0x1b);
        }
        bytes.push(ctrl_byte);
        return Ok(bytes);
    }

    // No ctrl
    let ch_byte = if shift && ch.is_ascii_alphabetic() {
        ch.to_ascii_uppercase() as u8
    } else {
        ch as u8
    };

    if alt {
        bytes.push(0x1b);
    }
    bytes.push(ch_byte);

    Ok(bytes)
}

/// Encode a simple key (no CSI sequence) with alt prefix.
fn encode_simple_key(base: &[u8], ctrl: bool, _shift: bool, alt: bool) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    if alt {
        bytes.push(0x1b);
    }
    if ctrl {
        // For keys like cr, space — ctrl doesn't meaningfully change them,
        // but we still send the base byte with alt prefix if needed.
        // Ctrl+Enter and Ctrl+Space are special in some terminals but
        // there's no universal encoding. Just send the base.
    }
    bytes.extend_from_slice(base);
    Ok(bytes)
}

/// Encode a CSI key sequence with optional modifier parameter.
///
/// For unmodified keys like Up: `\x1b[A`
/// For modified keys like Ctrl+Up: `\x1b[1;5A`
///
/// The `suffix` is the final byte(s) of the sequence.
/// For tilde-terminated sequences like Delete (`3~`), the number is part of suffix.
fn encode_csi_key(suffix: &[u8], ctrl: bool, shift: bool, alt: bool) -> Result<Vec<u8>, String> {
    let modifier = xterm_modifier(ctrl, shift, alt);
    let mut bytes = vec![0x1b, b'['];

    if modifier > 1 {
        // For sequences like `\x1b[A` → `\x1b[1;5A`
        // For sequences like `\x1b[3~` → `\x1b[3;5~`
        if suffix.len() > 1 && suffix.last() == Some(&b'~') {
            // Tilde-terminated: e.g. `3~` → `3;5~`
            bytes.extend_from_slice(&suffix[..suffix.len() - 1]);
            bytes.push(b';');
            bytes.extend_from_slice(modifier.to_string().as_bytes());
            bytes.push(b'~');
        } else {
            // Letter-terminated: e.g. `A` → `1;5A`
            bytes.push(b'1');
            bytes.push(b';');
            bytes.extend_from_slice(modifier.to_string().as_bytes());
            bytes.extend_from_slice(suffix);
        }
    } else {
        bytes.extend_from_slice(suffix);
    }

    Ok(bytes)
}

/// Encode F1-F4 which use SS3 when unmodified, CSI when modified.
///
/// Unmodified: `\x1bOP` (SS3 P)
/// Modified: `\x1b[1;5P` (CSI with modifier)
fn encode_ss3_or_csi(
    ss3_suffix: &[u8],
    csi_suffix: &[u8],
    ctrl: bool,
    shift: bool,
    alt: bool,
) -> Result<Vec<u8>, String> {
    let modifier = xterm_modifier(ctrl, shift, alt);
    if modifier > 1 {
        // Use CSI with modifier
        let mut bytes = vec![0x1b, b'[', b'1', b';'];
        bytes.extend_from_slice(modifier.to_string().as_bytes());
        bytes.extend_from_slice(csi_suffix);
        Ok(bytes)
    } else {
        // Use SS3
        let mut bytes = vec![0x1b, b'O'];
        bytes.extend_from_slice(ss3_suffix);
        Ok(bytes)
    }
}

/// Encode F5-F12 style keys: `\x1b[N~` or `\x1b[N;modifier~`
fn encode_csi_tilde(num: u8, ctrl: bool, shift: bool, alt: bool) -> Result<Vec<u8>, String> {
    let modifier = xterm_modifier(ctrl, shift, alt);
    let mut bytes = vec![0x1b, b'['];
    bytes.extend_from_slice(num.to_string().as_bytes());
    if modifier > 1 {
        bytes.push(b';');
        bytes.extend_from_slice(modifier.to_string().as_bytes());
    }
    bytes.push(b'~');
    Ok(bytes)
}

/// Calculate the xterm modifier parameter.
///
/// | Modifier Combo       | Value |
/// |----------------------|-------|
/// | (none)               | 1     |
/// | Shift                | 2     |
/// | Alt                  | 3     |
/// | Shift+Alt            | 4     |
/// | Ctrl                 | 5     |
/// | Ctrl+Shift           | 6     |
/// | Ctrl+Alt             | 7     |
/// | Ctrl+Shift+Alt       | 8     |
fn xterm_modifier(ctrl: bool, shift: bool, alt: bool) -> u8 {
    1 + (shift as u8) + (alt as u8) * 2 + (ctrl as u8) * 4
}

/// Merge adjacent Bytes groups into single groups.
fn merge_bytes(groups: &mut Vec<KeyGroup>) {
    let mut merged = Vec::with_capacity(groups.len());

    for group in groups.drain(..) {
        match (&mut merged.last_mut(), &group) {
            (Some(KeyGroup::Bytes(existing)), KeyGroup::Bytes(new)) => {
                existing.extend_from_slice(new);
            }
            _ => {
                merged.push(group);
            }
        }
    }

    *groups = merged;
}
