//! Just enough JSON writing for machine-readable summaries, without a
//! dependency.

pub fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
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

/// An array of already-encoded values.
pub fn array(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(","))
}

/// An object of already-encoded values.
pub fn object<'a>(fields: impl IntoIterator<Item = (&'a str, String)>) -> String {
    let fields: Vec<String> = fields
        .into_iter()
        .map(|(k, v)| format!("{}:{v}", string(k)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped() {
        assert_eq!(string("a\"b\\c\n\u{1}"), r#""a\"b\\c\n\u0001""#);
    }

    #[test]
    fn objects_and_arrays_nest() {
        let inner = array(["1".to_string(), string("x")]);
        assert_eq!(object([("k", inner)]), r#"{"k":[1,"x"]}"#);
    }
}
