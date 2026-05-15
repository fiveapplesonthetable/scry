//! Android init.rc parser.
//!
//! init.rc has two main top-level construct kinds:
//!
//!   - service NAME PATH ARGS...
//!         user ...
//!         group ...
//!         class ...
//!
//!   - on EVENT
//!         start NAME
//!         stop NAME
//!         setprop K V
//!         ...
//!
//! Indented lines extend the previous top-level block. We emit one
//! InitService symbol per `service` decl and Call refs for `start NAME` /
//! `stop NAME` / `restart NAME`.

use crate::{make_ref, make_symbol};
use scry_lang::{RawRef, RawSymbol};
use scry_store::{RefKind, SymbolKind};

pub fn parse(source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let src = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut syms = Vec::new();
    let mut refs = Vec::new();
    let mut current_service: Option<String> = None;
    let mut current_on: Option<String> = None;
    let mut byte_offset: u32 = 0;
    for (i, raw_line) in src.lines().enumerate() {
        let line_no = (i + 1) as u32;
        let line_len = raw_line.len() as u32;
        let indented = raw_line.starts_with(|c: char| c == ' ' || c == '\t');
        let body = raw_line.trim();

        if body.is_empty() || body.starts_with('#') {
            byte_offset += line_len + 1; continue;
        }

        if !indented {
            // top-level: starts a new block
            let mut words = body.split_whitespace();
            let kw = words.next().unwrap_or("");
            match kw {
                "service" => {
                    if let Some(name) = words.next() {
                        let col = raw_line.find(name).unwrap_or(0) as u32 + 1;
                        let path: Vec<&str> = words.collect();
                        let scope = if let Some(first) = path.first() {
                            vec!["service".to_string(), first.to_string()]
                        } else {
                            vec!["service".to_string()]
                        };
                        syms.push(make_symbol(
                            name.to_string(), SymbolKind::InitService,
                            line_no, col, byte_offset + col - 1,
                            byte_offset + col - 1 + name.len() as u32, scope,
                        ));
                        current_service = Some(name.to_string());
                        current_on = None;
                    }
                }
                "on" => {
                    let event = body[3..].trim().to_string();
                    current_on = Some(event);
                    current_service = None;
                }
                _ => {
                    // unknown top-level — reset context
                    current_service = None;
                    current_on = None;
                }
            }
        } else {
            // continuation of the previous block
            let mut words = body.split_whitespace();
            let kw = words.next().unwrap_or("");
            let target = words.next();
            match (kw, target) {
                ("start", Some(name)) | ("stop", Some(name)) | ("restart", Some(name)) => {
                    let col = raw_line.find(name).unwrap_or(0) as u32 + 1;
                    let scope = match (&current_service, &current_on) {
                        (Some(s), _) => vec!["service".to_string(), s.clone()],
                        (_, Some(e)) => vec!["on".to_string(), e.clone()],
                        _ => Vec::new(),
                    };
                    refs.push(make_ref(
                        name.to_string(), RefKind::Call,
                        line_no, col, byte_offset + col - 1,
                        byte_offset + col - 1 + name.len() as u32, scope,
                    ));
                }
                _ => {}
            }
        }

        byte_offset += line_len + 1;
    }
    (syms, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_and_starts() {
        let src = br#"
service zygote /system/bin/app_process -Xzygote /system/bin --zygote
    class main
    user root
    socket zygote stream 666 root system

service surfaceflinger /system/bin/surfaceflinger
    class main
    user system

on early-init
    start ueventd

on boot
    start zygote
    start surfaceflinger
"#;
        let (syms, refs) = parse(src);
        let snames: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(snames.contains(&"zygote"));
        assert!(snames.contains(&"surfaceflinger"));
        let rnames: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(rnames.contains(&"zygote"));
        assert!(rnames.contains(&"surfaceflinger"));
        assert!(rnames.contains(&"ueventd"));
    }
}
