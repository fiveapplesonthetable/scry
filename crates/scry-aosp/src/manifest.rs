//! AndroidManifest.xml parser. Extracts package + component declarations
//! (activity/service/receiver/provider) and emits one ManifestComponent
//! symbol per declared component. Each component's `android:name` value is
//! the symbol name; references back to that name from Java/Kotlin code
//! (typically as a class) match by string.

use crate::{make_ref, make_symbol};
use quick_xml::events::Event;
use quick_xml::Reader;
use scry_lang::{RawRef, RawSymbol};
use scry_store::{RefKind, SymbolKind};

pub fn parse(source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let mut reader = Reader::from_reader(source);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut syms = Vec::new();
    let mut refs = Vec::new();
    let mut package: Option<String> = None;

    let component_tags = [
        "activity", "service", "receiver", "provider", "application",
        "uses-permission", "permission",
    ];

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Err(_) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = e.local_name();
                let tag = std::str::from_utf8(local.as_ref()).unwrap_or("").to_string();
                if tag == "manifest" {
                    for attr in e.attributes().flatten() {
                        let key = std::str::from_utf8(attr.key.local_name().as_ref())
                            .unwrap_or("")
                            .to_string();
                        if key == "package" {
                            if let Ok(v) = attr.unescape_value() {
                                package = Some(v.to_string());
                            }
                        }
                    }
                    continue;
                }
                if component_tags.contains(&tag.as_str()) {
                    let mut name: Option<String> = None;
                    for attr in e.attributes().flatten() {
                        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                        if key == "android:name" || key == "name" {
                            if let Ok(v) = attr.unescape_value() {
                                name = Some(v.to_string());
                            }
                        }
                    }
                    if let Some(mut n) = name {
                        // Resolve relative class names against package
                        if n.starts_with('.') {
                            if let Some(pkg) = &package {
                                n = format!("{pkg}{n}");
                            }
                        }
                        let pos = reader.buffer_position();
                        syms.push(make_symbol(
                            n.clone(),
                            SymbolKind::ManifestComponent,
                            (pos as u32).max(1),
                            1,
                            pos as u32,
                            pos as u32 + n.len() as u32,
                            vec![tag.clone()],
                        ));
                        // Also emit an InheritFrom-ish ref so a Java class
                        // search will associate.
                        refs.push(make_ref(
                            n.clone(),
                            RefKind::InheritFrom,
                            (pos as u32).max(1),
                            1,
                            pos as u32,
                            pos as u32 + n.len() as u32,
                            vec![tag.clone()],
                        ));
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }
    (syms, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_components() {
        let xml = br#"
            <manifest package="com.foo">
                <application android:name=".MyApp">
                    <activity android:name=".ui.Main"/>
                    <service android:name="com.foo.MyService"/>
                    <receiver android:name=".RcvA"/>
                </application>
            </manifest>
        "#;
        let (syms, _refs) = parse(xml);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"com.foo.MyApp"));
        assert!(names.contains(&"com.foo.ui.Main"));
        assert!(names.contains(&"com.foo.MyService"));
        assert!(names.contains(&"com.foo.RcvA"));
    }
}
