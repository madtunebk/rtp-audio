//! The little JSON that `--json` prints, for programs (like a GUI) that run rtp-audio.

/// `text` as a JSON string, quotes included.
pub fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn strings_are_escaped() {
        assert_eq!(super::string("HDA \"NVidia\", HDMI\\1\n\u{1}"), r#""HDA \"NVidia\", HDMI\\1\n\u0001""#);
    }
}
