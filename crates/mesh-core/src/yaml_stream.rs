//! Event-driven YAML deserializer with an explicit per-file budget (plan 4.9).
//!
//! `serde_yaml` 0.9 loads a document's *whole* event list (one heap-allocated
//! event per scalar/collection, plus its position) before any visitor runs. Measured
//! with `scripts/bench/yaml_md_peak.py`, that is 24–34× the file size for an
//! OpenAPI/AsyncAPI spec or a Spring property file (`docs/quality.md`, 4.9). This
//! module drives the same serde visitors straight from the `saphyr-parser` event
//! stream instead: events are pulled one at a time, so the parser holds only its
//! scanner state, and a visitor's output is the only thing that grows with the file.
//!
//! The one thing a stream cannot do on its own is replay an alias (`*name`). The
//! events of every anchored node (`&name`) are therefore recorded as they stream by
//! (scalars borrowed from the input, not copied) and replayed when an alias is
//! deserialized. That is also the only way a small file can make a visitor do
//! unbounded work: `serde_yaml`'s own guard (`jump` in its `de.rs`) only errors once
//! alias *jumps* exceed 100× the event count, so one big anchor replayed by many
//! one-line aliases passes it and grows the output as anchor size × alias count
//! (a 6 KB file measured at 78 MB, a 1.5 MB one killed at 2 GB). Here both costs
//! are charged to one per-file budget, [`alias_budget`] units for a `len`-byte
//! input: one unit per event appended to an anchor recording, one per event
//! replayed by an alias. Exceeding it fails the document with
//! [`Error::BudgetExceeded`] (callers already treat a failed document as
//! contributing nothing). A file without aliases never spends any.
//!
//! Scalar resolution, tag handling (`!Tag` → enum), the recursion limit (128) and
//! "skipping a value never follows its aliases" are ported from `serde_yaml` 0.9
//! (`de.rs`, MIT/Apache-2.0) so the existing visitors see the same calls they did.

use libyaml::{Raw, ScalarStyle};
use serde::de::{self, DeserializeSeed, Deserializer, Visitor};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::num::ParseIntError;
use std::rc::Rc;

/// Units of alias work allowed per input byte (see the module docs).
pub const ALIAS_BUDGET_PER_BYTE: usize = 1;
/// Floor of the alias budget, so small files keep room for ordinary anchors.
pub const MIN_ALIAS_BUDGET: usize = 64 * 1024;
/// Same nesting limit as `serde_yaml` (`remaining_depth: 128`).
const RECURSION_LIMIT: usize = 128;

/// The alias budget of a `len`-byte document.
pub fn alias_budget(len: usize) -> usize {
    len.saturating_mul(ALIAS_BUDGET_PER_BYTE)
        .max(MIN_ALIAS_BUDGET)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The YAML itself is malformed.
    Scan(String),
    /// Anchor recording + alias replay went over [`alias_budget`].
    BudgetExceeded { budget: usize },
    /// `from_str` on a stream holding more than one document.
    MoreThanOneDocument,
    /// Anything a visitor or the structure reported.
    Message(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Scan(m) => write!(f, "YAML syntax error: {m}"),
            Error::BudgetExceeded { budget } => write!(
                f,
                "YAML anchor/alias expansion exceeds the per-file budget ({budget} events)"
            ),
            Error::MoreThanOneDocument => f.write_str(
                "deserializing from YAML containing more than one document is not supported",
            ),
            Error::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Deserializes a stream that must hold exactly one document, like
/// `serde_yaml::from_str` (an empty stream deserializes from "no value").
pub fn from_str<'a, T: de::Deserialize<'a>>(content: &'a str) -> Result<T> {
    let mut de = De::new(content)?;
    let value = match de.start_document()? {
        true => {
            let v = T::deserialize(&mut de)?;
            de.end_document()?;
            v
        }
        false => return T::deserialize(EmptyStream),
    };
    if de.start_document()? {
        return Err(Error::MoreThanOneDocument);
    }
    Ok(value)
}

/// Deserializes only the first document of a stream (later documents are never
/// parsed), like `serde_yaml::Deserializer::from_str(content).next()`.
/// `Ok(None)` for a stream with no document at all.
pub fn first_document<'a, S: DeserializeSeed<'a>>(
    content: &'a str,
    seed: S,
) -> Result<Option<S::Value>> {
    let mut de = De::new(content)?;
    if !de.start_document()? {
        return Ok(None);
    }
    let v = seed.deserialize(&mut de)?;
    de.end_document()?;
    Ok(Some(v))
}

/// One node event, anchors stripped (they are resolved at record time).
#[derive(Debug, Clone)]
enum Ev<'a> {
    Scalar {
        value: Cow<'a, str>,
        style: ScalarStyle,
        tag: Option<Rc<str>>,
    },
    SeqStart(Option<Rc<str>>),
    SeqEnd,
    MapStart(Option<Rc<str>>),
    MapEnd,
    Alias(usize),
    /// End of document / stream reached inside a node (truncated input).
    End,
}

struct Recording<'a> {
    id: usize,
    events: Vec<Ev<'a>>,
    depth: usize,
}

struct Replay<'a> {
    events: Rc<[Ev<'a>]>,
    pos: usize,
}

struct De<'a> {
    parser: libyaml::Parser<'a>,
    peeked: Option<Ev<'a>>,
    replay: Vec<Replay<'a>>,
    recording: Vec<Recording<'a>>,
    /// Anchor name -> id of its latest definition (a name may be redefined).
    anchor_ids: HashMap<Box<[u8]>, usize>,
    anchors: HashMap<usize, Rc<[Ev<'a>]>>,
    spent: usize,
    budget: usize,
    depth: usize,
    /// Set by `EnumAccess::variant_seed`: the next node is a tagged value being
    /// read through its tag (serde_yaml's `current_enum`).
    tagged: bool,
}

impl<'a> De<'a> {
    fn new(content: &'a str) -> Result<Self> {
        Ok(Self {
            parser: libyaml::Parser::new(content)?,
            peeked: None,
            replay: Vec::new(),
            recording: Vec::new(),
            anchor_ids: HashMap::new(),
            anchors: HashMap::new(),
            spent: 0,
            budget: alias_budget(content.len()),
            depth: 0,
            tagged: false,
        })
    }

    fn charge(&mut self, units: usize) -> Result<()> {
        self.spent = self.spent.saturating_add(units);
        if self.spent > self.budget {
            return Err(Error::BudgetExceeded {
                budget: self.budget,
            });
        }
        Ok(())
    }

    /// Advances the live stream to the next document's root. `false` at stream end.
    fn start_document(&mut self) -> Result<bool> {
        loop {
            match self.parser.next()? {
                Raw::StreamEnd => return Ok(false),
                Raw::DocumentStart => return Ok(true),
                _ => {}
            }
        }
    }

    /// Consumes the live stream up to (and including) the current document's end.
    fn end_document(&mut self) -> Result<()> {
        if let Some(ev) = self.peeked.take() {
            if !matches!(ev, Ev::End) {
                return Err(Error::Message("trailing YAML node in document".into()));
            }
            return Ok(());
        }
        match self.parser.next()? {
            Raw::DocumentEnd | Raw::StreamEnd => Ok(()),
            _ => Err(Error::Message("trailing YAML node in document".into())),
        }
    }

    fn define(&mut self, anchor: Option<Box<[u8]>>) -> usize {
        match anchor {
            None => 0,
            Some(name) => {
                let id = self.anchor_ids.len() + 1;
                self.anchor_ids.insert(name, id);
                id
            }
        }
    }

    /// Pulls the next live node event, recording it into every open anchor.
    fn live(&mut self) -> Result<Ev<'a>> {
        let (ev, anchor) = loop {
            break match self.parser.next()? {
                Raw::Scalar {
                    anchor,
                    tag,
                    value,
                    style,
                } => (Ev::Scalar { value, style, tag }, self.define(anchor)),
                Raw::SequenceStart { anchor, tag } => (Ev::SeqStart(tag), self.define(anchor)),
                Raw::MappingStart { anchor, tag } => (Ev::MapStart(tag), self.define(anchor)),
                Raw::SequenceEnd => (Ev::SeqEnd, 0),
                Raw::MappingEnd => (Ev::MapEnd, 0),
                Raw::Alias(name) => match self.anchor_ids.get(&name) {
                    Some(&id) => (Ev::Alias(id), 0),
                    None => {
                        return Err(Error::Message(format!(
                            "unknown anchor {}",
                            String::from_utf8_lossy(&name)
                        )))
                    }
                },
                Raw::DocumentEnd | Raw::StreamEnd => (Ev::End, 0),
                Raw::StreamStart | Raw::DocumentStart => continue,
            };
        };

        // Feed the open recordings, then close the innermost ones that end here.
        let open = self.recording.len();
        if open > 0 {
            self.charge(open)?;
            for rec in &mut self.recording {
                rec.events.push(ev.clone());
                match ev {
                    Ev::SeqStart(_) | Ev::MapStart(_) => rec.depth += 1,
                    Ev::SeqEnd | Ev::MapEnd => rec.depth = rec.depth.saturating_sub(1),
                    _ => {}
                }
            }
            while self.recording.last().is_some_and(|r| r.depth == 0) {
                if let Some(done) = self.recording.pop() {
                    self.anchors.insert(done.id, Rc::from(done.events));
                }
            }
        }
        if anchor != 0 {
            self.charge(1)?;
            match ev {
                Ev::SeqStart(_) | Ev::MapStart(_) => self.recording.push(Recording {
                    id: anchor,
                    events: vec![ev.clone()],
                    depth: 1,
                }),
                _ => {
                    self.anchors.insert(anchor, Rc::from(vec![ev.clone()]));
                }
            }
        }
        Ok(ev)
    }

    fn fill(&mut self) -> Result<()> {
        if self.peeked.is_some() {
            return Ok(());
        }
        while let Some(frame) = self.replay.last_mut() {
            if let Some(ev) = frame.events.get(frame.pos) {
                frame.pos += 1;
                self.peeked = Some(ev.clone());
                return Ok(());
            }
            self.replay.pop();
        }
        let ev = self.live()?;
        self.peeked = Some(ev);
        Ok(())
    }

    fn next(&mut self) -> Result<Ev<'a>> {
        self.fill()?;
        self.peeked
            .take()
            .ok_or_else(|| Error::Message("unexpected end of YAML".into()))
    }

    fn peek_is_end_of_collection(&mut self) -> Result<bool> {
        self.fill()?;
        Ok(matches!(self.peeked, Some(Ev::SeqEnd | Ev::MapEnd)))
    }

    fn follow(&mut self, id: usize) -> Result<()> {
        let events = self
            .anchors
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::Message(format!("unknown YAML anchor id {id}")))?;
        self.charge(events.len())?;
        self.replay.push(Replay { events, pos: 0 });
        Ok(())
    }

    /// Consumes one node without following its aliases (serde_yaml's `ignore_any`).
    fn skip_node(&mut self) -> Result<()> {
        let mut depth = 0usize;
        loop {
            match self.next()? {
                Ev::Scalar { .. } | Ev::Alias(_) => {}
                Ev::SeqStart(_) | Ev::MapStart(_) => depth += 1,
                Ev::SeqEnd | Ev::MapEnd => depth = depth.saturating_sub(1),
                Ev::End => return Err(Error::Message("unexpected end of YAML".into())),
            }
            if depth == 0 {
                return Ok(());
            }
        }
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > RECURSION_LIMIT {
            return Err(Error::Message("recursion limit exceeded".into()));
        }
        Ok(())
    }
}

/// `serde_yaml`'s `parse_tag`: a local tag (`!Foo`) names an enum variant.
fn enum_tag(tag: &Option<Rc<str>>, tagged_already: bool) -> Option<Rc<str>> {
    if tagged_already {
        return None;
    }
    let t = tag.as_ref()?;
    let rest = t.strip_prefix('!')?;
    Some(if rest.is_empty() {
        Rc::clone(t)
    } else {
        Rc::from(rest)
    })
}

impl<'de, 'a> Deserializer<'de> for &mut De<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = std::mem::take(&mut self.tagged);
        loop {
            let ev = self.next()?;
            match ev {
                Ev::Alias(id) => {
                    self.follow(id)?;
                    continue;
                }
                Ev::Scalar {
                    ref value,
                    style,
                    ref tag,
                } => {
                    if let Some(t) = enum_tag(tag, tagged_already) {
                        self.peeked = Some(ev);
                        return visitor.visit_enum(EnumAccess { de: self, tag: t });
                    }
                    return visit_scalar(visitor, value, style, tag.as_deref(), tagged_already);
                }
                Ev::SeqStart(ref tag) | Ev::MapStart(ref tag) => {
                    if let Some(t) = enum_tag(tag, tagged_already) {
                        self.peeked = Some(ev);
                        return visitor.visit_enum(EnumAccess { de: self, tag: t });
                    }
                    let is_map = matches!(ev, Ev::MapStart(_));
                    self.enter()?;
                    let mut access = Access {
                        de: &mut *self,
                        done: false,
                    };
                    let value = if is_map {
                        visitor.visit_map(&mut access)?
                    } else {
                        visitor.visit_seq(&mut access)?
                    };
                    let done = access.done;
                    if !done {
                        // The visitor stopped early: serde_yaml reports this as an
                        // invalid length once the rest is drained.
                        return Err(Error::Message(
                            "visitor did not consume the whole YAML collection".into(),
                        ));
                    }
                    self.depth -= 1;
                    return Ok(value);
                }
                Ev::SeqEnd | Ev::MapEnd => {
                    return Err(Error::Message("unexpected end of YAML collection".into()))
                }
                Ev::End => return visitor.visit_none(),
            }
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.tagged = false;
        self.skip_node()?;
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier
    }
}

/// Map and sequence access over the same stream (the end event tells them apart).
struct Access<'s, 'a> {
    de: &'s mut De<'a>,
    done: bool,
}

impl<'s, 'a> Access<'s, 'a> {
    fn at_end(&mut self) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        if self.de.peek_is_end_of_collection()? {
            self.de.peeked = None;
            self.done = true;
            return Ok(true);
        }
        Ok(false)
    }
}

impl<'de, 's, 'a> de::SeqAccess<'de> for &mut Access<'s, 'a> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.at_end()? {
            return Ok(None);
        }
        seed.deserialize(&mut *self.de).map(Some)
    }
}

impl<'de, 's, 'a> de::MapAccess<'de> for &mut Access<'s, 'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.at_end()? {
            return Ok(None);
        }
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        seed.deserialize(&mut *self.de)
    }
}

struct EnumAccess<'s, 'a> {
    de: &'s mut De<'a>,
    tag: Rc<str>,
}

impl<'de, 's, 'a> de::EnumAccess<'de> for EnumAccess<'s, 'a> {
    type Error = Error;
    type Variant = &'s mut De<'a>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let variant = seed.deserialize(de::value::StrDeserializer::<Error>::new(&self.tag))?;
        self.de.tagged = true;
        Ok((variant, self.de))
    }
}

impl<'de, 's, 'a> de::VariantAccess<'de> for &'s mut De<'a> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        de::Deserialize::deserialize(self)
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }
}

/// An empty stream: `serde_yaml` deserializes it as "no value".
struct EmptyStream;

impl<'de> Deserializer<'de> for EmptyStream {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_none()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// The libyaml event parser (`unsafe-libyaml`, the parser `serde_yaml` 0.9
/// itself runs), pulled one event at a time instead of through `serde_yaml`'s
/// whole-document loader. Keeping the same parser keeps the same accepted
/// syntax: the pure-Rust YAML 1.2 parsers tried first (`saphyr-parser`,
/// `yaml-rust2`) reject the flow sequence closed at its key's indentation that
/// libyaml accepts and real `compose.yaml` files use.
///
/// All `unsafe` is confined here; it mirrors `serde_yaml`'s own
/// `libyaml/parser.rs` wrapper.
mod libyaml {
    use super::{Error, Result};
    use std::borrow::Cow;
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::rc::Rc;
    use unsafe_libyaml as sys;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ScalarStyle {
        Plain,
        SingleQuoted,
        DoubleQuoted,
        Literal,
        Folded,
    }

    pub(super) enum Raw<'a> {
        StreamStart,
        StreamEnd,
        DocumentStart,
        DocumentEnd,
        Alias(Box<[u8]>),
        Scalar {
            anchor: Option<Box<[u8]>>,
            tag: Option<Rc<str>>,
            value: Cow<'a, str>,
            style: ScalarStyle,
        },
        SequenceStart {
            anchor: Option<Box<[u8]>>,
            tag: Option<Rc<str>>,
        },
        SequenceEnd,
        MappingStart {
            anchor: Option<Box<[u8]>>,
            tag: Option<Rc<str>>,
        },
        MappingEnd,
    }

    pub(super) struct Parser<'a> {
        /// Boxed so the initialized parser never moves (libyaml keeps pointers
        /// into its own buffers).
        sys: Box<MaybeUninit<sys::yaml_parser_t>>,
        input: &'a str,
        failed: bool,
    }

    impl<'a> Parser<'a> {
        pub(super) fn new(input: &'a str) -> Result<Self> {
            let mut sys = Box::new(MaybeUninit::<sys::yaml_parser_t>::uninit());
            // SAFETY: `yaml_parser_initialize` fully initializes the parser it is
            // given (it zeroes it first). The input pointer stays valid for 'a,
            // which outlives the parser (it is dropped with `Self`).
            unsafe {
                let parser = sys.as_mut_ptr();
                if sys::yaml_parser_initialize(parser).fail {
                    // Nothing was allocated that `yaml_parser_delete` could free
                    // safely; report and never touch it again.
                    std::mem::forget(sys);
                    return Err(Error::Scan("libyaml parser initialization failed".into()));
                }
                sys::yaml_parser_set_encoding(parser, sys::YAML_UTF8_ENCODING);
                sys::yaml_parser_set_input_string(parser, input.as_ptr(), input.len() as _);
            }
            Ok(Self {
                sys,
                input,
                failed: false,
            })
        }

        pub(super) fn next(&mut self) -> Result<Raw<'a>> {
            if self.failed {
                return Err(Error::Scan("YAML parser already failed".into()));
            }
            let mut event = MaybeUninit::<sys::yaml_event_t>::uninit();
            // SAFETY: the parser was initialized in `new` and is only used through
            // `&mut self`; `yaml_parser_parse` initializes `event` on success, and
            // the event is converted (copying or re-borrowing everything it
            // points to) before `yaml_event_delete` frees it.
            unsafe {
                let parser = self.sys.as_mut_ptr();
                if sys::yaml_parser_parse(parser, event.as_mut_ptr()).fail {
                    self.failed = true;
                    return Err(parse_error(&*parser));
                }
                let event = event.as_mut_ptr();
                let raw = convert(&*event, self.input);
                sys::yaml_event_delete(event);
                raw
            }
        }
    }

    impl Drop for Parser<'_> {
        fn drop(&mut self) {
            // SAFETY: initialized in `new` (a failed initialization never builds
            // a `Parser`), deleted exactly once here.
            unsafe { sys::yaml_parser_delete(self.sys.as_mut_ptr()) }
        }
    }

    /// SAFETY: `parser` is an initialized parser whose last call failed.
    unsafe fn parse_error(parser: &sys::yaml_parser_t) -> Error {
        let problem = if parser.problem.is_null() {
            "libyaml parser failed".to_string()
        } else {
            // SAFETY: libyaml problem strings are static NUL-terminated literals.
            unsafe { CStr::from_ptr(parser.problem) }
                .to_string_lossy()
                .into_owned()
        };
        let mark = parser.problem_mark;
        Error::Scan(format!(
            "{problem} at line {} column {}",
            mark.line + 1,
            mark.column + 1
        ))
    }

    /// SAFETY: `ptr` is null or a NUL-terminated string owned by a live event.
    unsafe fn c_bytes(ptr: *const u8) -> Option<Box<[u8]>> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: per the function contract.
        Some(Box::from(unsafe { CStr::from_ptr(ptr.cast()) }.to_bytes()))
    }

    /// SAFETY: as `c_bytes`.
    unsafe fn c_tag(ptr: *const u8) -> Result<Option<Rc<str>>> {
        // SAFETY: per the function contract.
        match unsafe { c_bytes(ptr) } {
            None => Ok(None),
            Some(bytes) => std::str::from_utf8(&bytes)
                .map(|s| Some(Rc::from(s)))
                .map_err(|_| Error::Scan("YAML tag is not UTF-8".into())),
        }
    }

    /// The scalar's text, borrowed from the input when the source bytes are the
    /// value itself (a plain or simple quoted scalar), copied otherwise.
    fn scalar_text<'a>(
        value: &[u8],
        input: &'a str,
        start: usize,
        end: usize,
    ) -> Result<Cow<'a, str>> {
        if let Some(repr) = input.get(start..end) {
            for inner in [Some(repr), repr.get(1..repr.len().saturating_sub(1))]
                .into_iter()
                .flatten()
            {
                if inner.as_bytes() == value {
                    return Ok(Cow::Borrowed(inner));
                }
            }
        }
        String::from_utf8(value.to_vec())
            .map(Cow::Owned)
            .map_err(|_| Error::Scan("YAML scalar is not UTF-8".into()))
    }

    /// SAFETY: `event` was just produced by `yaml_parser_parse` and not deleted.
    unsafe fn convert<'a>(event: &sys::yaml_event_t, input: &'a str) -> Result<Raw<'a>> {
        // SAFETY (all union reads): `type_` names the active union member.
        unsafe {
            Ok(match event.type_ {
                sys::YAML_STREAM_START_EVENT => Raw::StreamStart,
                sys::YAML_STREAM_END_EVENT | sys::YAML_NO_EVENT => Raw::StreamEnd,
                sys::YAML_DOCUMENT_START_EVENT => Raw::DocumentStart,
                sys::YAML_DOCUMENT_END_EVENT => Raw::DocumentEnd,
                sys::YAML_ALIAS_EVENT => Raw::Alias(
                    c_bytes(event.data.alias.anchor)
                        .ok_or_else(|| Error::Scan("YAML alias without anchor".into()))?,
                ),
                sys::YAML_SCALAR_EVENT => {
                    let s = event.data.scalar;
                    let value: &[u8] = if s.value.is_null() || s.length == 0 {
                        &[]
                    } else {
                        std::slice::from_raw_parts(s.value, s.length as usize)
                    };
                    Raw::Scalar {
                        anchor: c_bytes(s.anchor),
                        tag: c_tag(s.tag)?,
                        value: scalar_text(
                            value,
                            input,
                            event.start_mark.index as usize,
                            event.end_mark.index as usize,
                        )?,
                        style: match s.style {
                            sys::YAML_SINGLE_QUOTED_SCALAR_STYLE => ScalarStyle::SingleQuoted,
                            sys::YAML_DOUBLE_QUOTED_SCALAR_STYLE => ScalarStyle::DoubleQuoted,
                            sys::YAML_LITERAL_SCALAR_STYLE => ScalarStyle::Literal,
                            sys::YAML_FOLDED_SCALAR_STYLE => ScalarStyle::Folded,
                            _ => ScalarStyle::Plain,
                        },
                    }
                }
                sys::YAML_SEQUENCE_START_EVENT => Raw::SequenceStart {
                    anchor: c_bytes(event.data.sequence_start.anchor),
                    tag: c_tag(event.data.sequence_start.tag)?,
                },
                sys::YAML_SEQUENCE_END_EVENT => Raw::SequenceEnd,
                sys::YAML_MAPPING_START_EVENT => Raw::MappingStart {
                    anchor: c_bytes(event.data.mapping_start.anchor),
                    tag: c_tag(event.data.mapping_start.tag)?,
                },
                sys::YAML_MAPPING_END_EVENT => Raw::MappingEnd,
                _ => return Err(Error::Scan("unknown libyaml event".into())),
            })
        }
    }
}

// ---- Scalar resolution, ported from serde_yaml 0.9 `de.rs` ----------------

const TAG_BOOL: &str = "tag:yaml.org,2002:bool";
const TAG_INT: &str = "tag:yaml.org,2002:int";
const TAG_FLOAT: &str = "tag:yaml.org,2002:float";
const TAG_NULL: &str = "tag:yaml.org,2002:null";

fn visit_scalar<'de, V: Visitor<'de>>(
    visitor: V,
    v: &str,
    style: ScalarStyle,
    tag: Option<&str>,
    tagged_already: bool,
) -> Result<V::Value> {
    if let (Some(tag), false) = (tag, tagged_already) {
        if tag == TAG_BOOL {
            return match parse_bool(v) {
                Some(b) => visitor.visit_bool(b),
                None => Err(de::Error::invalid_value(
                    de::Unexpected::Str(v),
                    &"a boolean",
                )),
            };
        } else if tag == TAG_INT {
            return match visit_int(visitor, v) {
                Ok(result) => result,
                Err(_) => Err(de::Error::invalid_value(
                    de::Unexpected::Str(v),
                    &"an integer",
                )),
            };
        } else if tag == TAG_FLOAT {
            return match parse_f64(v) {
                Some(f) => visitor.visit_f64(f),
                None => Err(de::Error::invalid_value(de::Unexpected::Str(v), &"a float")),
            };
        } else if tag == TAG_NULL {
            return match parse_null(v) {
                Some(()) => visitor.visit_unit(),
                None => Err(de::Error::invalid_value(de::Unexpected::Str(v), &"null")),
            };
        } else if tag.starts_with('!') && style == ScalarStyle::Plain {
            return visit_untagged_scalar(visitor, v);
        }
    } else if style == ScalarStyle::Plain {
        return visit_untagged_scalar(visitor, v);
    }
    visitor.visit_str(v)
}

fn visit_untagged_scalar<'de, V: Visitor<'de>>(visitor: V, v: &str) -> Result<V::Value> {
    if v.is_empty() || parse_null(v).is_some() {
        return visitor.visit_unit();
    }
    if let Some(b) = parse_bool(v) {
        return visitor.visit_bool(b);
    }
    let visitor = match visit_int(visitor, v) {
        Ok(result) => return result,
        Err(visitor) => visitor,
    };
    if !digits_but_not_number(v) {
        if let Some(f) = parse_f64(v) {
            return visitor.visit_f64(f);
        }
    }
    visitor.visit_str(v)
}

fn parse_null(scalar: &str) -> Option<()> {
    match scalar {
        "null" | "Null" | "NULL" | "~" => Some(()),
        _ => None,
    }
}

fn parse_bool(scalar: &str) -> Option<bool> {
    match scalar {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

fn radix_prefixed<T>(
    unsigned: &str,
    from_str_radix: fn(&str, u32) -> std::result::Result<T, ParseIntError>,
) -> Option<Option<T>> {
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = unsigned.strip_prefix(prefix) {
            if rest.starts_with(['+', '-']) {
                return Some(None);
            }
            if let Ok(int) = from_str_radix(rest, radix) {
                return Some(Some(int));
            }
        }
    }
    None
}

fn parse_unsigned_int<T>(
    scalar: &str,
    from_str_radix: fn(&str, u32) -> std::result::Result<T, ParseIntError>,
) -> Option<T> {
    let unpositive = scalar.strip_prefix('+').unwrap_or(scalar);
    if let Some(found) = radix_prefixed(unpositive, from_str_radix) {
        return found;
    }
    if unpositive.starts_with(['+', '-']) {
        return None;
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(unpositive, 10).ok()
}

fn parse_negative_int<T>(
    scalar: &str,
    from_str_radix: fn(&str, u32) -> std::result::Result<T, ParseIntError>,
) -> Option<T> {
    for (prefix, radix) in [("-0x", 16), ("-0o", 8), ("-0b", 2)] {
        if let Some(rest) = scalar.strip_prefix(prefix) {
            if let Ok(int) = from_str_radix(&format!("-{rest}"), radix) {
                return Some(int);
            }
        }
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(scalar, 10).ok()
}

fn parse_f64(scalar: &str) -> Option<f64> {
    let unpositive = if let Some(unpositive) = scalar.strip_prefix('+') {
        if unpositive.starts_with(['+', '-']) {
            return None;
        }
        unpositive
    } else {
        scalar
    };
    if let ".inf" | ".Inf" | ".INF" = unpositive {
        return Some(f64::INFINITY);
    }
    if let "-.inf" | "-.Inf" | "-.INF" = scalar {
        return Some(f64::NEG_INFINITY);
    }
    if let ".nan" | ".NaN" | ".NAN" = scalar {
        return Some(f64::NAN.copysign(1.0));
    }
    if let Ok(float) = unpositive.parse::<f64>() {
        if float.is_finite() {
            return Some(float);
        }
    }
    None
}

fn digits_but_not_number(scalar: &str) -> bool {
    // Leading zero(s) followed by numeric characters is a string (YAML 1.2).
    let scalar = scalar.strip_prefix(['-', '+']).unwrap_or(scalar);
    scalar.len() > 1 && scalar.starts_with('0') && scalar[1..].bytes().all(|b| b.is_ascii_digit())
}

fn visit_int<'de, V: Visitor<'de>>(
    visitor: V,
    v: &str,
) -> std::result::Result<Result<V::Value>, V> {
    if let Some(int) = parse_unsigned_int(v, u64::from_str_radix) {
        return Ok(visitor.visit_u64(int));
    }
    if let Some(int) = parse_negative_int(v, i64::from_str_radix) {
        return Ok(visitor.visit_i64(int));
    }
    if let Some(int) = parse_unsigned_int(v, u128::from_str_radix) {
        return Ok(visitor.visit_u128(int));
    }
    if let Some(int) = parse_negative_int(v, i128::from_str_radix) {
        return Ok(visitor.visit_i128(int));
    }
    Err(visitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::IgnoredAny;

    fn skip(content: &str) -> Result<()> {
        from_str::<IgnoredAny>(content).map(|_| ())
    }

    /// Deserializes into `serde_yaml::Value` through both deserializers.
    fn both(
        yaml: &str,
    ) -> (
        Result<serde_yaml::Value>,
        std::result::Result<serde_yaml::Value, serde_yaml::Error>,
    ) {
        (
            from_str::<serde_yaml::Value>(yaml),
            serde_yaml::from_str::<serde_yaml::Value>(yaml),
        )
    }

    #[test]
    fn matches_serde_yaml_on_scalars_tags_and_aliases() {
        let cases = [
            "a: 1\nb: -2\nc: 0x1F\nd: 0o17\ne: 0b101\nf: 1.5\ng: 1e3\nh: .inf\ni: -.Inf\n",
            "a: true\nb: False\nc: yes\nd: ~\ne: null\nf:\ng: '1'\nh: \"true\"\n",
            "a: 007\nb: +12\nc: -0x10\nd: 18446744073709551615\ne: -9223372036854775808\n",
            "d: 340282366920938463463374607431768211455\n",
            "a: !!str 12\nb: !!int '12'\nc: !!float '1'\nd: !!bool 'true'\ne: !!null ''\n",
            "a: !Custom {x: 1}\nb: !Other 12\nc: ! plain\n",
            "base: &b {x: 1, y: [1, 2]}\nuse: *b\nlist: [*b, *b]\nk: &s scalar\nv: *s\n",
            "? [complex, key]\n: v\nplain: |\n  literal\n  block\nfolded: >\n  folded\n  text\n",
            "- 1\n- [a, {b: c}]\n- {d: [e, f]}\n",
            "",
            "just text",
            "&a [1, 2]",
        ];
        for yaml in cases {
            let (ours, theirs) = both(yaml);
            match (ours, theirs) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "yaml: {yaml:?}"),
                (Err(_), Err(_)) => {}
                (a, b) => panic!("yaml {yaml:?}: ours {a:?} vs serde_yaml {b:?}"),
            }
        }
    }

    #[test]
    fn errors_where_serde_yaml_errors() {
        for yaml in [
            "a: [1, 2",
            "a: *missing\n",
            "a: 1\n---\nb: 2\n",
            "a: !!int x\n",
        ] {
            let (ours, theirs) = both(yaml);
            assert!(theirs.is_err(), "serde_yaml accepted {yaml:?}");
            assert!(ours.is_err(), "stream accepted {yaml:?}: {ours:?}");
        }
    }

    #[test]
    fn first_document_ignores_later_documents() {
        let v: Option<serde_yaml::Value> =
            first_document("a: 1\n---\nb: [unclosed\n", std::marker::PhantomData)
                .expect("first doc");
        assert_eq!(v, serde_yaml::from_str("a: 1").ok());
        assert_eq!(
            first_document::<std::marker::PhantomData<serde_yaml::Value>>(
                "",
                std::marker::PhantomData
            ),
            Ok(None)
        );
    }

    #[test]
    fn nesting_past_the_recursion_limit_errors() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(skip(&deep).is_ok(), "skipping never recurses");
        assert!(from_str::<serde_yaml::Value>(&deep).is_err());
    }

    #[test]
    fn alias_fan_out_is_cut_by_the_budget() {
        // One anchored mapping of 2,000 keys replayed by 2,000 aliases: 4M replayed
        // events from a ~50 KB file. serde_yaml's jump limit (100 x events) lets
        // this through; the budget must not.
        let mut yaml = String::from("base: &big\n");
        for i in 0..2000 {
            yaml.push_str(&format!("  k{i}: v\n"));
        }
        yaml.push_str("refs:\n");
        for i in 0..2000 {
            yaml.push_str(&format!("  p{i}: *big\n"));
        }
        assert!(serde_yaml::from_str::<serde_yaml::Value>(&yaml).is_ok());
        assert_eq!(
            from_str::<serde_yaml::Value>(&yaml),
            Err(Error::BudgetExceeded {
                budget: alias_budget(yaml.len())
            })
        );
        // Skipping the aliases (a visitor that ignores the values) costs nothing.
        assert!(skip(&yaml).is_ok());
    }

    #[test]
    fn classic_billion_laughs_is_cut_by_the_budget() {
        let mut yaml = String::from("l0: &l0 [lol]\n");
        for level in 1..10 {
            let refs = vec![format!("*l{}", level - 1); 9].join(", ");
            yaml.push_str(&format!("l{level}: &l{level} [{refs}]\n"));
        }
        assert!(matches!(
            from_str::<serde_yaml::Value>(&yaml),
            Err(Error::BudgetExceeded { .. })
        ));
    }

    #[test]
    fn ordinary_anchor_reuse_stays_within_budget() {
        let mut yaml = String::from("defaults: &d {timeout: 3000, retries: 3}\nservices:\n");
        for i in 0..500 {
            yaml.push_str(&format!("  s{i}: {{<<: *d, url: http://s{i}}}\n"));
        }
        let (ours, theirs) = both(&yaml);
        assert_eq!(ours.expect("stream"), theirs.expect("serde_yaml"));
    }
}
