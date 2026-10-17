use smol_str::SmolStr;

use super::{SdblToken, SdblTokenKind};

pub(super) struct StringsRun {
    pub tokens: Vec<SdblToken>,
    pub end_pos: usize,
}

pub(super) fn scan(input: &str, start_pos: usize) -> StringsRun {
    let bytes = input.as_bytes();
    let mut pos = start_pos;

    if pos >= bytes.len() || bytes[pos] != b'"' {
        return StringsRun { tokens: Vec::new(), end_pos: pos };
    }

    let mut tokens = Vec::new();

    tokens.push(make_string_token(input, pos, pos + 1));
    pos += 1;

    let body_start = pos;
    let mut closed = false;
    while pos < bytes.len() {
        match bytes[pos] {
            b'"' if pos + 1 < bytes.len() && bytes[pos + 1] == b'"' => pos += 2,
            b'"' => {
                if body_start < pos {
                    tokens.push(make_string_token(input, body_start, pos));
                }
                tokens.push(make_string_token(input, pos, pos + 1));
                pos += 1;
                closed = true;
                break;
            }
            _ => pos += 1,
        }
    }
    if !closed && body_start < pos {
        tokens.push(make_string_token(input, body_start, pos));
    }

    StringsRun { tokens, end_pos: pos }
}

fn make_string_token(input: &str, start: usize, end: usize) -> SdblToken {
    SdblToken { kind: SdblTokenKind::String, text: SmolStr::new(&input[start..end]), offset: start }
}
