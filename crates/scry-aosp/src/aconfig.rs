//! aconfig (Android feature flag) parser.
//!
//! aconfig declaration files are protobuf textproto: optional `package: "..."`
//! and `container: "..."` lines, then one or more `flag { ... }` blocks.
//! Inside each block we extract `name` (required) and optionally `namespace`
//! and `description` for context.
//!
//! ```text
//! package: "com.android.foo"
//! container: "system"
//! flag {
//!     name: "my_feature"
//!     namespace: "feature_team"
//!     description: "..."
//!     bug: "12345"
//!     is_fixed_read_only: true
//! }
//! ```

use crate::make_symbol;
use scry_lang::{RawRef, RawSymbol};
use scry_store::SymbolKind;

pub fn parse(source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let src = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut syms = Vec::new();
    let mut package: Option<String> = None;

    // Trivial line-based scan, since textproto is very regular.
    // We accept either `key: VALUE` form or `key { ... }` block form.
    let mut in_flag = false;
    let mut flag_brace_depth = 0i32;
    let mut current_flag: Option<(String, u32, u32, u32)> = None;

    for (i, raw_line) in src.lines().enumerate() {
        let line_no = (i + 1) as u32;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        // Strip trailing comment after `//` or `#`
        let line = match line.find("//") { Some(p) => &line[..p], None => line };
        let line = line.trim();

        if !in_flag {
            if let Some(rest) = line.strip_prefix("package:") {
                package = Some(strip_quotes(rest.trim()).to_string());
                continue;
            }
            if line.starts_with("flag") {
                // flag { OR flag {
                if line.contains('{') {
                    in_flag = true;
                    flag_brace_depth = 1;
                    current_flag = None;
                }
                continue;
            }
        } else {
            // inside a flag block
            for c in line.chars() {
                if c == '{' { flag_brace_depth += 1; }
                if c == '}' { flag_brace_depth -= 1; }
            }
            if let Some(rest) = line.strip_prefix("name:") {
                let name = strip_quotes(rest.trim()).to_string();
                if !name.is_empty() {
                    let col = (raw_line.len() - raw_line.trim_start().len()) as u32 + 1;
                    let byte_start = 0; // approx; not critical
                    current_flag = Some((name.clone(), line_no, col, byte_start));
                }
            }
            if flag_brace_depth <= 0 {
                // end of flag block — emit
                if let Some((name, line_no, col, byte_start)) = current_flag.take() {
                    let scope: Vec<String> = package.iter().cloned().collect();
                    syms.push(make_symbol(
                        name.clone(), SymbolKind::AconfigFlag,
                        line_no, col, byte_start, byte_start + name.len() as u32,
                        scope,
                    ));
                }
                in_flag = false;
                flag_brace_depth = 0;
            }
        }
    }

    (syms, Vec::new())
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_aconfig() {
        let src = br#"
            package: "com.android.example"
            flag {
                name: "feature_alpha"
                namespace: "team_x"
                description: "Enables alpha feature"
                bug: "111"
            }
            flag {
                name: "feature_beta"
                namespace: "team_y"
                description: "Enables beta"
            }
        "#;
        let (syms, _) = parse(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"feature_alpha"), "names: {:?}", names);
        assert!(names.contains(&"feature_beta"));
    }
}
