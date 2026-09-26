//! Shallow, streaming views of AsyncAPI / OpenAPI documents.
//!
//! The extractors only need a handful of mapping *keys* (`channels`, `topics`,
//! `paths.<path>.<method>`). Deserializing into these shapes walks the YAML event
//! stream once and skips every other subtree as `IgnoredAny`, instead of
//! materializing a full `serde_yaml::Value` tree (every schema, example and
//! description of a 1.5 MB spec) just to read its top-level keys.
//!
//! Each view accepts any YAML shape: a field of the wrong shape (e.g. `channels`
//! written as a list, or a root that is not a mapping) yields an empty view
//! rather than failing the document, the same "skip what isn't a
//! mapping/sequence" behaviour the `Value::get()` / `as_mapping()` /
//! `as_sequence()` / `as_str()` checks had. Those accessors see through YAML
//! tags (`!Tag {..}`), so every view here unwraps a tagged node to its content
//! too.
//!
//! The views are driven by [`from_yaml`], i.e. `mesh_core::yaml_stream`
//! (plan 4.9): libyaml events pulled one at a time rather than `serde_yaml`'s
//! whole-document event list (24-29x a 1.5 MB spec), and a per-file budget on
//! anchor/alias expansion (an alias fan-out used to grow `paths` as anchor size x
//! alias count: a 30 KB `openapi.yaml` measured at 1.85 GB).
//!
//! One deliberate difference from the `Value` tree: a duplicate mapping key
//! somewhere in the document (typically deep inside a schema) no longer
//! rejects the whole spec. A duplicated channel/path key is reported once per
//! occurrence, in document order.

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;

/// Reads a spec view from a single-document YAML stream (same contract as
/// `serde_yaml::from_str`) through the streaming, budgeted driver.
pub(crate) fn from_yaml<'a, T: Deserialize<'a>>(
    content: &'a str,
) -> Result<T, mesh_core::yaml_stream::Error> {
    mesh_core::yaml_stream::from_str(content)
}

/// No `derive(Deserialize)` on the two root shapes: a derived struct also
/// accepts a *sequence* (fields by position), so a root `- {a: 1}` list would
/// have been read as `channels: [a]`. `Value::get("channels")` on a non-mapping
/// root was `None`; the hand-written visitors below keep that.
#[derive(Debug, Default)]
pub(crate) struct AsyncApiShape {
    pub channels: KeyList,
    pub topics: StrList,
}

#[derive(Debug, Default)]
pub(crate) struct OpenApiShape {
    pub paths: NestedKeyList,
}

/// String keys of a mapping, in document order; values skipped.
#[derive(Debug, Default)]
pub(crate) struct KeyList(pub Vec<String>);

/// String entries of a sequence, in order; non-string entries skipped.
#[derive(Debug, Default)]
pub(crate) struct StrList(pub Vec<String>);

/// String keys of a mapping, each with the string keys of its (mapping) value.
#[derive(Debug, Default)]
pub(crate) struct NestedKeyList(pub Vec<(String, Vec<String>)>);

/// `Some` for a string scalar, `None` for anything else (fully consumed).
struct MaybeStr(Option<String>);

fn drain_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// Implements the "any other shape" visitor methods as "consume and return
/// `$empty`", so a wrong-shaped field never errors the whole document.
macro_rules! other_shapes_are_empty {
    ($ty:ty, $empty:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_unit<E: de::Error>(self) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_none<E: de::Error>(self) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_i128<E: de::Error>(self, _: i128) -> Result<$ty, E> {
            Ok($empty)
        }
        fn visit_u128<E: de::Error>(self, _: u128) -> Result<$ty, E> {
            Ok($empty)
        }
        /// A tagged node (`!Tag content`): read through the tag, as `Value`'s
        /// accessors did.
        fn visit_enum<A: de::EnumAccess<'de>>(self, data: A) -> Result<$ty, A::Error> {
            let (IgnoredAny, variant) = data.variant::<IgnoredAny>()?;
            de::VariantAccess::newtype_variant::<$ty>(variant)
        }
    };
}

/// Root mapping visitor shared by the two spec shapes: `$field => $slot` pairs
/// are read with their view type, every other key's value is skipped, and any
/// non-mapping root is an empty shape.
macro_rules! root_shape {
    ($shape:ident { $($field:literal => $slot:ident),+ $(,)? }) => {
        impl<'de> Deserialize<'de> for $shape {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $shape;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("any YAML value")
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<$shape, A::Error> {
                        let mut out = $shape::default();
                        while let Some(MaybeStr(key)) = map.next_key::<MaybeStr>()? {
                            match key.as_deref() {
                                $(Some($field) => out.$slot = map.next_value()?,)+
                                _ => {
                                    map.next_value::<IgnoredAny>()?;
                                }
                            }
                        }
                        Ok(out)
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<$shape, A::Error> {
                        drain_seq(seq).map(|()| $shape::default())
                    }
                    fn visit_str<E: de::Error>(self, _: &str) -> Result<$shape, E> {
                        Ok($shape::default())
                    }
                    other_shapes_are_empty!($shape, $shape::default());
                }
                d.deserialize_any(V)
            }
        }
    };
}

root_shape!(AsyncApiShape { "channels" => channels, "topics" => topics });
root_shape!(OpenApiShape { "paths" => paths });

impl<'de> Deserialize<'de> for MaybeStr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MaybeStr;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any YAML value")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<MaybeStr, E> {
                Ok(MaybeStr(Some(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<MaybeStr, E> {
                Ok(MaybeStr(Some(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<MaybeStr, A::Error> {
                drain_seq(seq).map(|()| MaybeStr(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<MaybeStr, A::Error> {
                drain_map(map).map(|()| MaybeStr(None))
            }
            other_shapes_are_empty!(MaybeStr, MaybeStr(None));
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for KeyList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = KeyList;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any YAML value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<KeyList, A::Error> {
                let mut keys = Vec::new();
                while let Some(MaybeStr(key)) = map.next_key::<MaybeStr>()? {
                    map.next_value::<IgnoredAny>()?;
                    keys.extend(key);
                }
                Ok(KeyList(keys))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<KeyList, A::Error> {
                drain_seq(seq).map(|()| KeyList::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<KeyList, E> {
                Ok(KeyList::default())
            }
            other_shapes_are_empty!(KeyList, KeyList::default());
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for StrList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrList;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any YAML value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StrList, A::Error> {
                let mut out = Vec::new();
                while let Some(MaybeStr(entry)) = seq.next_element::<MaybeStr>()? {
                    out.extend(entry);
                }
                Ok(StrList(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<StrList, A::Error> {
                drain_map(map).map(|()| StrList::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<StrList, E> {
                Ok(StrList::default())
            }
            other_shapes_are_empty!(StrList, StrList::default());
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for NestedKeyList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = NestedKeyList;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any YAML value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<NestedKeyList, A::Error> {
                let mut out = Vec::new();
                while let Some(MaybeStr(key)) = map.next_key::<MaybeStr>()? {
                    match key {
                        Some(k) => {
                            let KeyList(inner) = map.next_value::<KeyList>()?;
                            out.push((k, inner));
                        }
                        None => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(NestedKeyList(out))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<NestedKeyList, A::Error> {
                drain_seq(seq).map(|()| NestedKeyList::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<NestedKeyList, E> {
                Ok(NestedKeyList::default())
            }
            other_shapes_are_empty!(NestedKeyList, NestedKeyList::default());
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asyncapi_shape_reads_keys_and_skips_everything_else() {
        let yaml = r#"
asyncapi: 2.6.0
info: {title: x, version: "1"}
channels:
  orders.created:
    subscribe: {message: {payload: {type: object}}}
  7: numeric-key-skipped
  users.updated: {}
topics: [a, 1, {b: c}, d]
"#;
        let shape: AsyncApiShape = from_yaml(yaml).expect("shape");
        assert_eq!(shape.channels.0, ["orders.created", "users.updated"]);
        assert_eq!(shape.topics.0, ["a", "d"]);
    }

    #[test]
    fn wrong_shaped_fields_are_empty_not_errors() {
        let shape: AsyncApiShape = from_yaml("channels: [x, y]\ntopics: {a: b}\n").expect("shape");
        assert!(shape.channels.0.is_empty());
        assert!(shape.topics.0.is_empty());
    }

    /// `Value`'s accessors read through tags; so do the views. A non-mapping
    /// root is empty (a derived struct would have read a list positionally).
    #[test]
    fn tags_are_transparent_and_non_mapping_roots_are_empty() {
        let yaml =
            "!Spec\nchannels: !Chans\n  ? !K orders\n  : {}\n  users: {}\ntopics: !T [a, !S b]\n";
        let shape: AsyncApiShape = from_yaml(yaml).expect("shape");
        assert_eq!(shape.channels.0, ["orders", "users"]);
        assert_eq!(shape.topics.0, ["a", "b"]);

        let listed: AsyncApiShape = from_yaml("- {a: 1}\n- [x, y]\n").expect("list root");
        assert!(listed.channels.0.is_empty());
        assert!(listed.topics.0.is_empty());
        let scalar: OpenApiShape = from_yaml("just text").expect("scalar root");
        assert!(scalar.paths.0.is_empty());
    }

    /// The `Value` tree agreed with these views on every shape below; the views
    /// are checked against it directly.
    #[test]
    fn views_match_value_accessors() {
        let docs = [
            "channels:\n  a: {}\n  1: x\n  !t b: {}\n  ? [c]\n  : {}\ntopics: [x, 2, !t y, {z: 1}]\n",
            "channels: !m {a: 1}\ntopics: !s [p]\n",
            "channels: [a]\ntopics: {a: b}\n",
            "!root {channels: {a: 1}, topics: [t]}\n",
            "channels: ~\ntopics: 3\n",
        ];
        for doc in docs {
            let value: serde_yaml::Value = serde_yaml::from_str(doc).expect("value");
            let keys: Vec<String> = value
                .get("channels")
                .and_then(|c| c.as_mapping())
                .map(|m| {
                    m.keys()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let items: Vec<String> = value
                .get("topics")
                .and_then(|c| c.as_sequence())
                .map(|s| {
                    s.iter()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let shape: AsyncApiShape = from_yaml(doc).expect("shape");
            assert_eq!(shape.channels.0, keys, "channels of {doc:?}");
            assert_eq!(shape.topics.0, items, "topics of {doc:?}");
        }
    }

    #[test]
    fn openapi_shape_reads_paths_and_methods() {
        let yaml = r#"
openapi: 3.0.0
paths:
  /users:
    get: {responses: {"200": {description: ok}}}
    post: {}
  /health: "not-a-mapping"
  ? [complex]
  : {get: {}}
components: {schemas: {User: {type: object}}}
"#;
        let shape: OpenApiShape = from_yaml(yaml).expect("shape");
        assert_eq!(
            shape.paths.0,
            vec![
                (
                    "/users".to_string(),
                    vec!["get".to_string(), "post".to_string()]
                ),
                ("/health".to_string(), vec![]),
            ]
        );
    }

    /// Both drivers must produce the same views (or both fail).
    fn assert_drivers_agree(yaml: &str) {
        let a_old = serde_yaml::from_str::<AsyncApiShape>(yaml).map(|s| (s.channels.0, s.topics.0));
        let a_new = from_yaml::<AsyncApiShape>(yaml).map(|s| (s.channels.0, s.topics.0));
        match (&a_old, &a_new) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "asyncapi view of {yaml:.200}"),
            (Err(_), Err(_)) => {}
            _ => panic!("asyncapi drivers disagree on {yaml:.200}: {a_old:?} vs {a_new:?}"),
        }
        let o_old = serde_yaml::from_str::<OpenApiShape>(yaml).map(|s| s.paths.0);
        let o_new = from_yaml::<OpenApiShape>(yaml).map(|s| s.paths.0);
        match (&o_old, &o_new) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "openapi view of {yaml:.200}"),
            (Err(_), Err(_)) => {}
            _ => panic!("openapi drivers disagree on {yaml:.200}: {o_old:?} vs {o_new:?}"),
        }
    }

    #[test]
    fn stream_driver_matches_serde_yaml_driver() {
        let docs = [
            "paths:\n  /a: &ops {get: {}, post: {}}\n  /b: *ops\n",
            "channels:\n  a: &c {x: 1}\n  b: *c\ntopics: &t [x, y]\n",
            "openapi: 3.0.0\npaths: {}\n---\npaths: {/x: {get: {}}}\n",
            "",
            "paths:\n  /a: {get: [unclosed\n",
            "paths:\n  /a:\n    get: {}\n  /a:\n    put: {}\n",
        ];
        for doc in docs {
            assert_drivers_agree(doc);
        }
    }

    /// Plan 4.9 security bound: one anchored mapping replayed under many paths
    /// is refused by the per-file budget instead of materializing
    /// anchor-size x alias-count method keys.
    #[test]
    fn alias_fan_out_in_paths_is_refused() {
        let mut yaml = String::from("openapi: 3.0.0\nx-base: &big\n");
        for i in 0..2000 {
            yaml.push_str(&format!("  m{i}: {{}}\n"));
        }
        yaml.push_str("paths:\n");
        for i in 0..2000 {
            yaml.push_str(&format!("  /p{i}: *big\n"));
        }
        assert!(matches!(
            from_yaml::<OpenApiShape>(&yaml),
            Err(mesh_core::yaml_stream::Error::BudgetExceeded { .. })
        ));
    }

    /// Opt-in sweep over real YAML (`MESH_YAML_EQUIV_DIR=~/.cache/mesh-golden
    /// cargo test --release -p mesh-parsers spec_views_match_on_dir -- --ignored`).
    #[test]
    #[ignore]
    fn spec_views_match_on_dir() {
        let Ok(dir) = std::env::var("MESH_YAML_EQUIV_DIR") else {
            return;
        };
        let mut stack = vec![std::path::PathBuf::from(dir)];
        let mut seen = 0usize;
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let path = entry.path();
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file()
                    && path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e == "yml" || e == "yaml")
                {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        assert_drivers_agree(&text);
                        seen += 1;
                    }
                }
            }
        }
        eprintln!("compared {seen} YAML files");
    }
}
