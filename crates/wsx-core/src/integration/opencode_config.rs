use std::io;

const SPEC: &str = "./wsx-tui-session.js";

/// Registers the wsx TUI adapter without reserializing the user's JSONC file.
/// Comments, trailing commas, and unrelated values remain byte-for-byte intact.
pub(crate) fn register_tui(content: &str) -> io::Result<String> {
    let source = if content.trim().is_empty() {
        "{}\n"
    } else {
        content
    };
    let open = source
        .find('{')
        .ok_or_else(|| io::Error::other("OpenCode tui.jsonc must be a JSON object"))?;
    let close = matching(source, open, '{', '}')?
        .ok_or_else(|| io::Error::other("unterminated OpenCode tui.jsonc object"))?;
    if !source[close + 1..].trim().is_empty() {
        return Err(io::Error::other(
            "invalid trailing content in OpenCode tui.jsonc",
        ));
    }

    if let Some(key) = find_string_key(source, "plugin") {
        let colon = source[key..close]
            .find(':')
            .map(|offset| key + offset)
            .ok_or_else(|| io::Error::other("invalid OpenCode plugin property"))?;
        let array_open = source[colon + 1..close]
            .find(|ch: char| !ch.is_whitespace())
            .map(|offset| colon + 1 + offset)
            .filter(|index| source.as_bytes()[*index] == b'[')
            .ok_or_else(|| io::Error::other("OpenCode tui.jsonc plugin must be an array"))?;
        let array_close = matching(source, array_open, '[', ']')?
            .ok_or_else(|| io::Error::other("unterminated OpenCode plugin array"))?;
        if source[array_open + 1..array_close].contains(&format!("\"{SPEC}\"")) {
            return Ok(source.to_string());
        }
        let body = &source[array_open + 1..array_close];
        let addition = if body.trim().is_empty() {
            format!("\"{SPEC}\"")
        } else if body.trim_end().ends_with(',') {
            format!(" \"{SPEC}\"")
        } else {
            format!(", \"{SPEC}\"")
        };
        let mut out = source.to_string();
        out.insert_str(array_close, &addition);
        return Ok(out);
    }

    let body = &source[open + 1..close];
    let addition = if body.trim().is_empty() || body.trim_end().ends_with(',') {
        format!("\n  \"plugin\": [\"{SPEC}\"]\n")
    } else {
        format!(",\n  \"plugin\": [\"{SPEC}\"]\n")
    };
    let mut out = source.to_string();
    out.insert_str(close, &addition);
    Ok(out)
}

fn find_string_key(source: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\"");
    source.match_indices(&needle).find_map(|(index, _)| {
        source[index + needle.len()..]
            .trim_start()
            .starts_with(':')
            .then_some(index)
    })
}

fn matching(source: &str, start: usize, open: char, close: char) -> io::Result<Option<usize>> {
    let mut depth = 0_u32;
    let mut string = false;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut position = chars
        .iter()
        .position(|(index, _)| *index == start)
        .unwrap_or(0);
    while position < chars.len() {
        let (index, ch) = chars[position];
        let next = chars.get(position + 1).map(|(_, ch)| *ch);
        if line_comment {
            if ch == '\n' {
                line_comment = false;
            }
        } else if block_comment {
            if ch == '*' && next == Some('/') {
                block_comment = false;
                position += 1;
            }
        } else if string {
            if ch == '"' && !escaped {
                string = false;
            }
            escaped = ch == '\\' && !escaped;
            if ch != '\\' {
                escaped = false;
            }
        } else if ch == '/' && next == Some('/') {
            line_comment = true;
            position += 1;
        } else if ch == '/' && next == Some('*') {
            block_comment = true;
            position += 1;
        } else if ch == '"' {
            string = true;
        } else if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("unbalanced JSONC"))?;
            if depth == 0 {
                return Ok(Some(index));
            }
        }
        position += 1;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_jsonc_and_is_idempotent() {
        let input = "{\n  // retained\n  \"theme\": \"dark\",\n  \"plugin\": [\"other\"],\n}\n";
        let once = register_tui(input).unwrap();
        assert!(once.contains("// retained"));
        assert!(once.contains("\"other\", \"./wsx-tui-session.js\""));
        assert_eq!(register_tui(&once).unwrap(), once);
    }

    #[test]
    fn rejects_wrong_plugin_shape() {
        assert!(register_tui("{\"plugin\": {}}\n").is_err());
    }
}
