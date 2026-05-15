//! Android API signature .txt parser (metalava format).
//!
//! Format (excerpt):
//!   package android.app {
//!       public class Activity extends ContextThemeWrapper {
//!           ctor public Activity();
//!           method public void finish();
//!           method public boolean isFinishing();
//!           field public static final int RESULT_OK = -1;
//!       }
//!       @Deprecated public class Foo { ... }
//!   }
//!
//! This is the canonical declaration of Android's public/system/test API
//! surface. Each class/interface/method/field becomes a symbol so users can
//! query the public API directly: `scry def Activity --lang api`.

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
    let mut package_stack: Vec<String> = Vec::new();
    let mut class_stack: Vec<String> = Vec::new();
    let mut byte_offset: u32 = 0;
    for (i, raw) in src.lines().enumerate() {
        let line_no = (i + 1) as u32;
        let line = raw.trim();
        let raw_len = raw.len() as u32;

        // closing brace pops a scope
        if line == "}" {
            if !class_stack.is_empty() {
                class_stack.pop();
            } else if !package_stack.is_empty() {
                package_stack.pop();
            }
            byte_offset += raw_len + 1;
            continue;
        }

        // strip annotations like @Deprecated @ColorInt
        let line = strip_annotations(line);

        if let Some(rest) = line.strip_prefix("package ") {
            if let Some(end) = rest.find('{') {
                let pkg = rest[..end].trim().to_string();
                package_stack.push(pkg);
            }
            byte_offset += raw_len + 1;
            continue;
        }

        // class / interface / enum / @interface
        if let Some((kw, after)) = split_after_kind(line) {
            if let Some(end) = after.find('{') {
                let header = &after[..end];
                // name is the first word
                let name = header.split_ascii_whitespace().next().unwrap_or("").to_string();
                let kind = match kw {
                    "class" | "record" => SymbolKind::Class,
                    "interface" => SymbolKind::Interface,
                    "enum" => SymbolKind::Enum,
                    "annotation" => SymbolKind::Annotation,
                    _ => SymbolKind::Class,
                };
                if !name.is_empty() {
                    let scope = combined_scope(&package_stack, &class_stack);
                    syms.push(make_symbol(
                        name.clone(), kind,
                        line_no, 1, byte_offset, byte_offset + name.len() as u32,
                        scope,
                    ));
                    // extends / implements -> InheritFrom refs
                    if let Some(pos) = header.find(" extends ") {
                        let rest = &header[pos + " extends ".len()..];
                        for base in rest.split(|c: char| c == ',' || c == ' ').filter(|s| !s.is_empty()) {
                            if base == "implements" { break; }
                            refs.push(make_ref(
                                base.to_string(), RefKind::InheritFrom,
                                line_no, 1, byte_offset, byte_offset + base.len() as u32,
                                vec![combined_scope(&package_stack, &class_stack).join("::"), name.clone()],
                            ));
                        }
                    }
                    class_stack.push(name);
                }
                byte_offset += raw_len + 1;
                continue;
            }
        }

        // member declarations: ctor / method / field / enum_constant / property
        if let Some(rest) = line.strip_prefix("ctor ") {
            if let Some(name) = extract_member_name(rest, true) {
                emit_member(&package_stack, &class_stack, &name,
                    SymbolKind::Constructor, line_no, byte_offset, &mut syms);
            }
        } else if let Some(rest) = line.strip_prefix("method ") {
            if let Some(name) = extract_member_name(rest, true) {
                emit_member(&package_stack, &class_stack, &name,
                    SymbolKind::Method, line_no, byte_offset, &mut syms);
            }
        } else if let Some(rest) = line.strip_prefix("field ") {
            if let Some(name) = extract_member_name(rest, false) {
                emit_member(&package_stack, &class_stack, &name,
                    SymbolKind::Field, line_no, byte_offset, &mut syms);
            }
        } else if let Some(rest) = line.strip_prefix("enum_constant ") {
            if let Some(name) = extract_member_name(rest, false) {
                emit_member(&package_stack, &class_stack, &name,
                    SymbolKind::EnumVariant, line_no, byte_offset, &mut syms);
            }
        } else if let Some(rest) = line.strip_prefix("property ") {
            if let Some(name) = extract_member_name(rest, false) {
                emit_member(&package_stack, &class_stack, &name,
                    SymbolKind::Field, line_no, byte_offset, &mut syms);
            }
        }

        byte_offset += raw_len + 1;
    }
    (syms, refs)
}

fn strip_annotations(mut s: &str) -> &str {
    while s.starts_with('@') {
        if let Some(p) = s.find(' ') {
            s = &s[p + 1..].trim_start();
        } else { break; }
    }
    s
}

fn split_after_kind(s: &str) -> Option<(&'static str, &str)> {
    for kw in ["public final class ", "public abstract class ", "public class ",
              "final class ", "abstract class ", "class ",
              "public interface ", "interface ",
              "public enum ", "enum ",
              "public @interface ", "@interface ",
              "public record ", "record "] {
        if let Some(rest) = s.strip_prefix(kw) {
            let normalized: &'static str = if kw.contains("interface") {
                if kw.contains("@") { "annotation" } else { "interface" }
            } else if kw.contains("enum") { "enum" }
              else if kw.contains("record") { "record" }
              else { "class" };
            return Some((normalized, rest));
        }
    }
    None
}

fn extract_member_name(rest: &str, is_method: bool) -> Option<String> {
    // method signature: "public void finish();"  → name is "finish"
    //                   "public boolean isFinishing();" → "isFinishing"
    //                   "public static int FOO = 1;" → for field
    // Find token before '(' for methods, before '=' or ';' for fields.
    let mut s = rest.trim_end_matches(';').trim();
    if is_method {
        let pidx = match s.find('(') { Some(p) => p, None => return None };
        s = &s[..pidx];
    } else {
        if let Some(eq) = s.find('=') {
            s = s[..eq].trim_end();
        }
    }
    let tokens: Vec<&str> = s.split(|c: char| c.is_whitespace()).filter(|t| !t.is_empty()).collect();
    tokens.last().map(|s| s.to_string())
}

fn combined_scope(pkg: &[String], cls: &[String]) -> Vec<String> {
    let mut v = pkg.to_vec();
    v.extend(cls.iter().cloned());
    v
}

fn emit_member(
    pkg: &[String], cls: &[String],
    name: &str, kind: SymbolKind,
    line: u32, byte_start: u32,
    syms: &mut Vec<RawSymbol>,
) {
    let scope = combined_scope(pkg, cls);
    syms.push(make_symbol(
        name.to_string(), kind,
        line, 1, byte_start, byte_start + name.len() as u32, scope,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_api() {
        let src = br#"
package android.app {
  public class Activity extends ContextWrapper implements ComponentCallbacks2 {
    ctor public Activity();
    method public void finish();
    method public boolean isFinishing();
    field public static final int RESULT_OK = -1;
  }
  public interface ActivityLifecycleCallbacks {
    method public void onActivityCreated(android.app.Activity, android.os.Bundle);
  }
}
"#;
        let (syms, _refs) = parse(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Activity"));
        assert!(names.contains(&"finish"));
        assert!(names.contains(&"isFinishing"));
        assert!(names.contains(&"RESULT_OK"));
        assert!(names.contains(&"ActivityLifecycleCallbacks"));
        assert!(names.contains(&"onActivityCreated"));
    }
}
