//! Shallow, streaming views of AsyncAPI / OpenAPI documents.
//!
//! The extractors only need a handful of mapping *keys* (`channels`, `topics`,
//! `paths.<path>.<method>`). Deserializing into these shapes walks the YAML event
//! stream once and skips every other subtree as `IgnoredAny`, instead of
//! materializing a full `serde_yaml::Value` tree (every schema, example and
//! description of a 1.5 MB spec) just to read its top-level keys.
//!
//! Each view accepts any YAML shape: a field of the wrong shape (e.g. `channels`
//! written as a list) yields an empty view rather than failing the document, the
//! same "skip what isn't a mapping/sequence" behaviour the `Value::as_mapping()` /
//! `as_sequence()` checks had.

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AsyncApiShape {
    #[serde(default)]
    pub channels: KeyList,
    #[serde(default)]
    pub topics: StrList,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OpenApiShape {
    #[serde(default)]
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
        fn visit_enum<A: de::EnumAccess<'de>>(self, data: A) -> Result<$ty, A::Error> {
            let (IgnoredAny, variant) = data.variant::<IgnoredAny>()?;
            de::VariantAccess::newtype_variant::<IgnoredAny>(variant)?;
            Ok($empty)
        }
    };
}

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
        let shape: AsyncApiShape = serde_yaml::from_str(yaml).expect("shape");
        assert_eq!(shape.channels.0, ["orders.created", "users.updated"]);
        assert_eq!(shape.topics.0, ["a", "d"]);
    }

    #[test]
    fn wrong_shaped_fields_are_empty_not_errors() {
        let shape: AsyncApiShape =
            serde_yaml::from_str("channels: [x, y]\ntopics: {a: b}\n").expect("shape");
        assert!(shape.channels.0.is_empty());
        assert!(shape.topics.0.is_empty());
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
        let shape: OpenApiShape = serde_yaml::from_str(yaml).expect("shape");
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
}
