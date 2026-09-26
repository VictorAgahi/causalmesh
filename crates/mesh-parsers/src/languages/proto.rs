use crate::decapitate::LanguageKind;
use crate::guard::AstGuard;
use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Tree};

pub struct ProtoExtractor;

/// `dependencies` mirrors every other language extractor's own relations
/// struct: `(node index, imported path)`, fed to `ContractGraph::add_dependency`
/// -> `find_dependents`. A `.proto` file's `import "other.proto";` is a
/// file-level declaration any node in the file can rely on (a message field
/// typed `google.type.Money`, say), so every import is attributed to every
/// node declared in the file, not to one specific declaration.
#[derive(Debug, Default)]
pub struct ProtoRelations {
    pub dependencies: Vec<(usize, CompactStr)>,
}

impl ProtoExtractor {
    /// Extracts with the canonical projection: RPC nodes are named `Service.Method`.
    pub fn extract(file_path: &Path, content: &str, repo_id: RepoId) -> Vec<ContractNode> {
        Self::extract_with_config(file_path, content, repo_id, true)
    }

    /// Extracts with configurable RPC method projection:
    /// `canonical_fqcn_projection = true` yields `Service.Method`, `false` yields bare method name.
    /// Test/ad-hoc entry point: parses `content` itself, at the query-time budget.
    /// Production indexing goes through [`Self::extract_with_parser`] via
    /// `PolyglotIndexer`, which parses once with `AstGuard::parse_with` at the (much
    /// larger) indexing budget, so a parse failure is visible instead of silently
    /// producing an empty result indistinguishable from a legitimately empty file.
    pub fn extract_with_config(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        canonical_fqcn_projection: bool,
    ) -> Vec<ContractNode> {
        AstGuard::with_parser(LanguageKind::Protobuf, |parser| {
            parser
                .parse(content, None)
                .map(|tree| {
                    Self::extract_with_parser(
                        file_path,
                        content,
                        repo_id,
                        canonical_fqcn_projection,
                        &tree,
                    )
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Extracts protobuf contract nodes from an already-parsed tree.
    pub fn extract_with_parser(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        canonical_fqcn_projection: bool,
        tree: &Tree,
    ) -> Vec<ContractNode> {
        Self::extract_with_relations(file_path, content, repo_id, canonical_fqcn_projection, tree).0
    }

    /// Same nodes as [`Self::extract_with_parser`], plus `import "other.proto";`
    /// dependencies.
    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        canonical_fqcn_projection: bool,
        tree: &Tree,
    ) -> (Vec<ContractNode>, ProtoRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut imports: Vec<CompactStr> = Vec::new();

        let source = content.as_bytes();
        let root = tree.root_node();
        let mut current_package = CompactStr::default();

        // 1. Locate top-level package declaration and imports
        for i in 0..root.child_count() {
            if let Some(child) = root.child(i) {
                match child.kind() {
                    "package" => {
                        if let Some(pkg) = Self::extract_package_name(child, source) {
                            current_package = pkg;
                        }
                    }
                    "import" => {
                        if let Some(path) = Self::extract_import_path(child, source) {
                            imports.push(path);
                        }
                    }
                    _ => {}
                }
            }
        }

        // 2. Walk services, RPC methods, and messages
        Self::walk_scope(
            root,
            source,
            &file_path,
            repo_id,
            &current_package,
            canonical_fqcn_projection,
            &mut nodes,
        );

        // A `.proto` import is a file-level declaration, not tied to one
        // specific message/service — but which node actually *uses* a given
        // import's types can't be determined without parsing the imported
        // file too (real cross-file type resolution, out of scope here). A
        // first version attributed every import to every node in the file;
        // caught by review: `ContractGraph::reconcile_edges` doesn't dedup
        // Imports edges across different `importer_id`s, so that produced up
        // to N-imports x M-nodes real edges in the graph, and
        // `find_dependents(import_path)` would return every node in the file
        // as a "dependent" even if only one actually used it — false-positive
        // fan-out, not just extra internal bookkeeping. Attributed to the
        // file's own first declared node instead: one edge per import,
        // traceable back to the file, without fabricating M-fold usage this
        // extractor has no evidence for.
        let mut relations = ProtoRelations::default();
        if !nodes.is_empty() {
            for path in imports {
                relations.dependencies.push((0, path));
            }
        }

        (nodes, relations)
    }

    /// `import "path/to/file.proto";` / `import public "...";` / `import weak "...";`
    /// — the imported path is always the (only) `string` child, quotes stripped.
    fn extract_import_path(import_node: Node, source: &[u8]) -> Option<CompactStr> {
        let mut cursor = import_node.walk();
        for child in import_node.children(&mut cursor) {
            if child.kind() == "string" {
                if let Ok(text) = child.utf8_text(source) {
                    // The protobuf grammar allows single- or double-quoted
                    // string literals for an import path (`choice('"', "'")`
                    // in tree-sitter-proto's own `string` rule) — only
                    // stripping double quotes left `import 'other.proto';`'s
                    // dependency key as the literal `'other.proto'`, quotes
                    // included, which would never match a real file path.
                    let unquoted = text.trim_matches(['"', '\'']);
                    if !unquoted.is_empty() {
                        return Some(CompactStr::new(unquoted));
                    }
                }
            }
        }
        None
    }

    fn extract_package_name(package_node: Node, source: &[u8]) -> Option<CompactStr> {
        for i in 0..package_node.child_count() {
            if let Some(child) = package_node.child(i) {
                if child.kind() == "full_ident" || child.kind() == "identifier" {
                    if let Ok(text) = child.utf8_text(source) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            return Some(CompactStr::new(trimmed));
                        }
                    }
                }
            }
        }

        if let Ok(raw) = package_node.utf8_text(source) {
            let pkg = raw
                .trim_start_matches("package")
                .trim_end_matches(';')
                .trim();
            if !pkg.is_empty() {
                return Some(CompactStr::new(pkg));
            }
        }

        None
    }

    fn walk_scope(
        parent: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package: &CompactStr,
        canonical_fqcn_projection: bool,
        nodes: &mut Vec<ContractNode>,
    ) {
        for i in 0..parent.child_count() {
            let child = match parent.child(i) {
                Some(c) => c,
                None => continue,
            };

            match child.kind() {
                "service" => {
                    let service_name = match Self::find_child_text(child, "service_name", source) {
                        Some(name) => name,
                        None => continue,
                    };

                    let line_start = child.start_position().row + 1;
                    let line_end = child.end_position().row + 1;
                    let signature = Self::extract_signature(child, source);
                    let docstring = Self::extract_preceding_docstring(child, source);

                    nodes.push(ContractNode {
                        id: 0,
                        name: service_name.clone(),
                        kind: NodeKind::GrpcService,
                        file_path: file_path.clone(),
                        line_start,
                        line_end,
                        package: package.clone(),
                        repo_id,
                        signature,
                        docstring,
                    });

                    // Walk RPC declarations within service body
                    for j in 0..child.child_count() {
                        if let Some(rpc_child) = child.child(j) {
                            if rpc_child.kind() == "rpc" {
                                if let Some(rpc_name) =
                                    Self::find_child_text(rpc_child, "rpc_name", source)
                                {
                                    let full_name = if canonical_fqcn_projection {
                                        format!("{service_name}.{rpc_name}")
                                    } else {
                                        rpc_name.to_string()
                                    };

                                    let rpc_line_start = rpc_child.start_position().row + 1;
                                    let rpc_line_end = rpc_child.end_position().row + 1;
                                    let rpc_sig = Self::extract_signature(rpc_child, source);
                                    let rpc_doc =
                                        Self::extract_preceding_docstring(rpc_child, source);

                                    nodes.push(ContractNode {
                                        id: 0,
                                        name: CompactStr::new(&full_name),
                                        kind: NodeKind::GrpcMethod,
                                        file_path: file_path.clone(),
                                        line_start: rpc_line_start,
                                        line_end: rpc_line_end,
                                        package: package.clone(),
                                        repo_id,
                                        signature: rpc_sig,
                                        docstring: rpc_doc,
                                    });
                                }
                            }
                        }
                    }
                }
                "message" => {
                    if let Some(message_name) = Self::find_child_text(child, "message_name", source)
                    {
                        let line_start = child.start_position().row + 1;
                        let line_end = child.end_position().row + 1;
                        let signature = Self::extract_signature(child, source);
                        let docstring = Self::extract_preceding_docstring(child, source);

                        nodes.push(ContractNode {
                            id: 0,
                            name: message_name,
                            kind: NodeKind::ProtoMessage,
                            file_path: file_path.clone(),
                            line_start,
                            line_end,
                            package: package.clone(),
                            repo_id,
                            signature,
                            docstring,
                        });

                        // Recursively walk message_body for nested messages
                        if let Some(body) = Self::find_child_by_kind(child, "message_body") {
                            Self::walk_scope(
                                body,
                                source,
                                file_path,
                                repo_id,
                                package,
                                canonical_fqcn_projection,
                                nodes,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn find_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == kind {
                    return Some(child);
                }
            }
        }
        None
    }

    fn find_child_text(parent: Node, child_kind: &str, source: &[u8]) -> Option<CompactStr> {
        let child = Self::find_child_by_kind(parent, child_kind)?;
        let text = child.utf8_text(source).ok()?.trim();
        if !text.is_empty() {
            Some(CompactStr::new(text))
        } else {
            None
        }
    }

    fn extract_signature(node: Node, source: &[u8]) -> Option<CompactStr> {
        let text = node.utf8_text(source).ok()?;
        let first_line = text.lines().next()?.trim();
        if !first_line.is_empty() {
            Some(CompactStr::new(first_line))
        } else {
            None
        }
    }

    fn extract_preceding_docstring(node: Node, source: &[u8]) -> Option<CompactStr> {
        let mut prev = node.prev_sibling()?;
        while prev.kind() == "\n" {
            prev = prev.prev_sibling()?;
        }
        if prev.kind() == "comment" {
            let text = prev.utf8_text(source).ok()?.trim();
            if !text.is_empty() {
                return Some(CompactStr::new(text));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Wire-format schema (milestone 4.6b)
// ---------------------------------------------------------------------------

/// Largest field number protobuf allows (`2^29 - 1`), which `reserved N to max;`
/// stands for.
pub const PROTO_MAX_FIELD_NUMBER: u32 = 536_870_911;

/// How many values a field carries on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoCardinality {
    /// Plain, `optional` or `required` field (`optional` only changes presence
    /// tracking, never the encoding).
    Singular,
    /// `repeated` field (packed or not).
    Repeated,
    /// `map<K, V>` field (on the wire, a repeated `{K key = 1; V value = 2;}`).
    Map,
}

impl ProtoCardinality {
    fn label(self) -> &'static str {
        match self {
            Self::Singular => "singular",
            Self::Repeated => "repeated",
            Self::Map => "map",
        }
    }
}

/// One field of a message, as far as the wire encoding is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoWireField {
    pub name: CompactStr,
    pub number: u32,
    pub cardinality: ProtoCardinality,
    /// Declared value type as written (`int32`, `string`, `foo.v1.Bar`, ...).
    pub value_type: CompactStr,
    /// `map<K, V>` key type; `None` for any other cardinality.
    pub map_key: Option<CompactStr>,
    /// Enclosing `oneof`, if any. Oneof members share the message's number space.
    pub oneof: Option<CompactStr>,
    pub line: usize,
}

impl ProtoWireField {
    /// `repeated int32`, `map<string, Foo>`, `string` — how the type reads in a report.
    pub fn type_label(&self) -> String {
        match (self.cardinality, &self.map_key) {
            (ProtoCardinality::Map, Some(key)) => format!("map<{key}, {}>", self.value_type),
            (ProtoCardinality::Repeated, _) => format!("repeated {}", self.value_type),
            _ => self.value_type.to_string(),
        }
    }
}

/// One message (nested messages are separate entries, named `Outer.Inner`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtoWireMessage {
    /// Dotted path inside the file (`Outer.Inner`), without the package.
    pub path: String,
    pub line: usize,
    pub fields: Vec<ProtoWireField>,
    /// Inclusive `reserved` number ranges (`reserved 5;` is `(5, 5)`,
    /// `reserved 9 to max;` is `(9, PROTO_MAX_FIELD_NUMBER)`).
    pub reserved_ranges: Vec<(u32, u32)>,
    pub reserved_names: Vec<CompactStr>,
}

impl ProtoWireMessage {
    pub fn is_number_reserved(&self, number: u32) -> bool {
        self.reserved_ranges
            .iter()
            .any(|&(lo, hi)| lo <= number && number <= hi)
    }

    pub fn is_name_reserved(&self, name: &str) -> bool {
        self.reserved_names.iter().any(|n| n.as_str() == name)
    }
}

/// Every message of one `.proto` file, plus the enum names it declares (so a
/// field typed with a file-local enum is known to be a varint, not a message).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtoWireSchema {
    pub messages: Vec<ProtoWireMessage>,
    /// Bare names of every enum declared in the file, at any nesting level.
    pub enum_names: Vec<CompactStr>,
}

/// Why a `.proto` source could not be turned into a [`ProtoWireSchema`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireSchemaError {
    /// Rejected by the same lexical guard the indexer applies (commandment 2).
    #[error("rejected by the parser guard: {0}")]
    Rejected(&'static str),
    /// Tree-sitter gave up (timeout) or produced no tree.
    #[error("tree-sitter parse failed or timed out")]
    ParseFailed,
    /// The file parsed with syntax errors: a partial schema would turn every
    /// field after the error into a false "deleted field" finding.
    #[error("syntax error near line {0}")]
    SyntaxError(usize),
}

/// The three wire-format rules of milestone 4.6b.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WireRule {
    /// A field number now names a different field (or a number the base
    /// version had `reserved` is used again).
    FieldNumberReused,
    /// Same number, same name, but a type or cardinality whose encoding the
    /// other side cannot decode (see [`wire_types_compatible`]).
    IncompatibleType,
    /// A field disappeared and neither its number nor its name is `reserved`.
    DeletedWithoutReserved,
}

impl WireRule {
    pub fn label(self) -> &'static str {
        match self {
            Self::FieldNumberReused => "field number reused",
            Self::IncompatibleType => "incompatible type change",
            Self::DeletedWithoutReserved => "field deleted without `reserved`",
        }
    }
}

/// One `WIRE_FORMAT_BREAKING_CHANGE` finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireBreakingChange {
    pub rule: WireRule,
    /// Message path (`Outer.Inner`).
    pub message: String,
    pub number: u32,
    /// Human-readable specifics (old/new name and type).
    pub detail: String,
    /// Line in the new version when the field still exists there, else in the base.
    pub line: usize,
}

/// Result of comparing a base schema to the working-tree schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireDiff {
    pub breaking: Vec<WireBreakingChange>,
    /// Messages present in the base but not in the new version (removed or
    /// renamed). Not a wire break by itself — a field still typed with it is
    /// caught as an incompatible type — but listed so the agent can check.
    pub removed_messages: Vec<String>,
}

/// Wire class of a field's value type. Two value types are compatible when a
/// reader expecting one decodes bytes written as the other into the same value
/// (protobuf "Updating A Message Type" rules), not merely when they share a wire type.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WireClass {
    /// `int32`, `uint32`, `int64`, `uint64`, `bool` and enums (plain varint;
    /// 64-bit values are truncated when read as 32-bit, like a C++ cast).
    Varint,
    /// `sint32`, `sint64` (zig-zag varint: not interchangeable with `Varint`).
    ZigZag,
    /// `fixed32`, `sfixed32`.
    Fixed32,
    /// `fixed64`, `sfixed64`.
    Fixed64,
    /// `float` — same wire type as `Fixed32`, but the bits mean something else.
    Float,
    /// `double` — same wire type as `Fixed64`, but the bits mean something else.
    Double,
    String,
    Bytes,
    /// A message type, by the name the field uses.
    Message(String),
}

fn classify_wire_type(ty: &str, enum_names: &[CompactStr]) -> WireClass {
    match ty {
        "int32" | "uint32" | "int64" | "uint64" | "bool" => WireClass::Varint,
        "sint32" | "sint64" => WireClass::ZigZag,
        "fixed32" | "sfixed32" => WireClass::Fixed32,
        "fixed64" | "sfixed64" => WireClass::Fixed64,
        "float" => WireClass::Float,
        "double" => WireClass::Double,
        "string" => WireClass::String,
        "bytes" => WireClass::Bytes,
        named => {
            let bare = named.rsplit('.').next().unwrap_or(named);
            if enum_names.iter().any(|e| e.as_str() == bare) {
                WireClass::Varint
            } else {
                WireClass::Message(named.trim_start_matches('.').to_string())
            }
        }
    }
}

/// `foo.v1.Bar`, `.foo.v1.Bar` and `Bar` all name the same message when one is
/// a dotted suffix of the other (relative vs fully-qualified reference).
fn same_type_name(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

/// Wire compatibility table for two value types (see `docs/mcp-tools.md`):
///
/// | Group | Compatible with each other |
/// |---|---|
/// | varint | `int32`, `uint32`, `int64`, `uint64`, `bool`, enums declared in the file |
/// | zig-zag | `sint32`, `sint64` |
/// | 32-bit | `fixed32`, `sfixed32` |
/// | 64-bit | `fixed64`, `sfixed64` |
/// | length-delimited | `string` ↔ `bytes` (valid UTF-8 only), `bytes` ↔ message, same message |
///
/// Everything else is incompatible, including `float`/`double` against the
/// fixed types they share a wire type with, `sint*` against the plain varints,
/// `string` against a message, and two different message types.
pub fn wire_types_compatible(
    old_ty: &str,
    old_enums: &[CompactStr],
    new_ty: &str,
    new_enums: &[CompactStr],
) -> bool {
    use WireClass as C;
    match (
        classify_wire_type(old_ty, old_enums),
        classify_wire_type(new_ty, new_enums),
    ) {
        (C::Message(a), C::Message(b)) => same_type_name(&a, &b),
        (C::String, C::Bytes) | (C::Bytes, C::String) => true,
        (C::Bytes, C::Message(_)) | (C::Message(_), C::Bytes) => true,
        (a, b) => a == b,
    }
}

/// Whether `old` and `new` (same number, same name) can decode each other's
/// bytes: same cardinality (a `repeated` ↔ singular or map ↔ non-map change is
/// always reported), then [`wire_types_compatible`] on the value (and map key).
pub fn wire_fields_compatible(
    old: &ProtoWireField,
    old_enums: &[CompactStr],
    new: &ProtoWireField,
    new_enums: &[CompactStr],
) -> bool {
    if old.cardinality != new.cardinality {
        return false;
    }
    let keys_ok = match (&old.map_key, &new.map_key) {
        (Some(a), Some(b)) => wire_types_compatible(a, old_enums, b, new_enums),
        (None, None) => true,
        _ => false,
    };
    keys_ok && wire_types_compatible(&old.value_type, old_enums, &new.value_type, new_enums)
}

/// Applies the three 4.6b rules to every message present in both versions.
/// Findings come out in base document order, then field-number order, so the
/// report is deterministic.
pub fn diff_wire_schemas(old: &ProtoWireSchema, new: &ProtoWireSchema) -> WireDiff {
    use std::collections::BTreeMap;

    let mut diff = WireDiff::default();
    let new_messages: BTreeMap<&str, &ProtoWireMessage> = new
        .messages
        .iter()
        .rev() // first declaration wins on a (malformed) duplicate path
        .map(|m| (m.path.as_str(), m))
        .collect();

    for old_msg in &old.messages {
        let Some(new_msg) = new_messages.get(old_msg.path.as_str()) else {
            diff.removed_messages.push(old_msg.path.clone());
            continue;
        };
        let start = diff.breaking.len();
        let old_by_num: BTreeMap<u32, &ProtoWireField> =
            old_msg.fields.iter().rev().map(|f| (f.number, f)).collect();
        let new_by_num: BTreeMap<u32, &ProtoWireField> =
            new_msg.fields.iter().rev().map(|f| (f.number, f)).collect();

        for (&number, &of) in &old_by_num {
            match new_by_num.get(&number) {
                Some(&nf) if nf.name != of.name => {
                    diff.breaking.push(WireBreakingChange {
                        rule: WireRule::FieldNumberReused,
                        message: old_msg.path.clone(),
                        number,
                        detail: format!(
                            "was `{}` (`{}`), now `{}` (`{}`): data written by either side is read as the other field",
                            of.name,
                            of.type_label(),
                            nf.name,
                            nf.type_label()
                        ),
                        line: nf.line,
                    });
                }
                Some(&nf) => {
                    if !wire_fields_compatible(of, &old.enum_names, nf, &new.enum_names) {
                        let why = if of.cardinality != nf.cardinality {
                            format!(
                                "cardinality {} -> {}",
                                of.cardinality.label(),
                                nf.cardinality.label()
                            )
                        } else {
                            "encodings are not interchangeable".to_string()
                        };
                        diff.breaking.push(WireBreakingChange {
                            rule: WireRule::IncompatibleType,
                            message: old_msg.path.clone(),
                            number,
                            detail: format!(
                                "`{}`: `{}` -> `{}` ({why})",
                                of.name,
                                of.type_label(),
                                nf.type_label()
                            ),
                            line: nf.line,
                        });
                    }
                }
                None => {
                    if !new_msg.is_number_reserved(number) && !new_msg.is_name_reserved(&of.name) {
                        diff.breaking.push(WireBreakingChange {
                            rule: WireRule::DeletedWithoutReserved,
                            message: old_msg.path.clone(),
                            number,
                            detail: format!(
                                "`{}` (`{}`) was removed; add `reserved {number};` and `reserved \"{}\";` so the number is never reused",
                                of.name,
                                of.type_label(),
                                of.name
                            ),
                            line: of.line,
                        });
                    }
                }
            }
        }

        for (&number, &nf) in &new_by_num {
            if !old_by_num.contains_key(&number) && old_msg.is_number_reserved(number) {
                diff.breaking.push(WireBreakingChange {
                    rule: WireRule::FieldNumberReused,
                    message: old_msg.path.clone(),
                    number,
                    detail: format!(
                        "number {number} was `reserved` in the base and is now `{}` (`{}`): old data for the deleted field is read as this one",
                        nf.name,
                        nf.type_label()
                    ),
                    line: nf.line,
                });
            }
        }
        diff.breaking[start..].sort_by_key(|c| (c.number, c.rule));
    }

    diff
}

impl ProtoExtractor {
    /// Parses `content` into its wire-level schema, behind the same lexical
    /// guard as indexing (binary sniff, 1,024-byte lines, nesting depth 64,
    /// schema size budget) and at the query-time parse budget, since this runs
    /// on an agent's synchronous `analyze_grpc` call.
    pub fn wire_schema(content: &str) -> Result<ProtoWireSchema, WireSchemaError> {
        let bytes = content.as_bytes();
        if bytes.len() as u64 > AstGuard::MAX_SCHEMA_FILE_SIZE_BYTES {
            return Err(WireSchemaError::Rejected(
                "file exceeds the 1.5 MB schema budget",
            ));
        }
        if AstGuard::looks_binary(bytes) {
            return Err(WireSchemaError::Rejected("binary content (null byte)"));
        }
        if bytes
            .split(|&b| b == b'\n')
            .any(|line| line.len() > AstGuard::MAX_LINE_LEN_BYTES)
        {
            return Err(WireSchemaError::Rejected("a line exceeds 1,024 bytes"));
        }
        if AstGuard::max_nesting_depth(bytes) > AstGuard::MAX_NESTING_DEPTH {
            return Err(WireSchemaError::Rejected("nesting depth exceeds 64"));
        }

        let outcome = AstGuard::parse_with(
            LanguageKind::Protobuf,
            content,
            AstGuard::QUERY_PARSE_TIMEOUT_MICROS,
            |tree| {
                let root = tree.root_node();
                if root.has_error() {
                    return Err(WireSchemaError::SyntaxError(Self::first_error_line(root)));
                }
                let mut schema = ProtoWireSchema::default();
                Self::collect_wire_scope(root, bytes, "", &mut schema);
                Ok(schema)
            },
        );
        outcome.into_option().ok_or(WireSchemaError::ParseFailed)?
    }

    fn first_error_line(root: Node) -> usize {
        let mut cursor = root.walk();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.is_error() || node.is_missing() {
                return node.start_position().row + 1;
            }
            if node.has_error() {
                let children: Vec<Node> = node.children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
        }
        root.start_position().row + 1
    }

    /// Walks the top level or a `message_body`, collecting messages (depth-first,
    /// document order) and enum names.
    fn collect_wire_scope(parent: Node, source: &[u8], prefix: &str, schema: &mut ProtoWireSchema) {
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            match child.kind() {
                "enum" => {
                    if let Some(name) = Self::find_child_text(child, "enum_name", source) {
                        schema.enum_names.push(name);
                    }
                }
                "message" => {
                    let Some(name) = Self::find_child_text(child, "message_name", source) else {
                        continue;
                    };
                    let path = if prefix.is_empty() {
                        name.to_string()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    let mut message = ProtoWireMessage {
                        path: path.clone(),
                        line: child.start_position().row + 1,
                        ..ProtoWireMessage::default()
                    };
                    let body = Self::find_child_by_kind(child, "message_body");
                    if let Some(body) = body {
                        Self::collect_message_members(body, source, &mut message);
                    }
                    schema.messages.push(message);
                    if let Some(body) = body {
                        Self::collect_wire_scope(body, source, &path, schema);
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_message_members(body: Node, source: &[u8], message: &mut ProtoWireMessage) {
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "field" => {
                    let repeated = Self::has_token(member, "repeated");
                    let cardinality = if repeated {
                        ProtoCardinality::Repeated
                    } else {
                        ProtoCardinality::Singular
                    };
                    if let Some(field) = Self::wire_field(member, source, cardinality, None, None) {
                        message.fields.push(field);
                    }
                }
                "map_field" => {
                    let key = Self::find_child_text(member, "key_type", source);
                    if key.is_some() {
                        if let Some(field) =
                            Self::wire_field(member, source, ProtoCardinality::Map, key, None)
                        {
                            message.fields.push(field);
                        }
                    }
                }
                "oneof" => {
                    let oneof_name = Self::find_child_text(member, "identifier", source);
                    let mut oneof_cursor = member.walk();
                    for oneof_member in member.children(&mut oneof_cursor) {
                        if oneof_member.kind() == "oneof_field" {
                            if let Some(field) = Self::wire_field(
                                oneof_member,
                                source,
                                ProtoCardinality::Singular,
                                None,
                                oneof_name.clone(),
                            ) {
                                message.fields.push(field);
                            }
                        }
                    }
                }
                "reserved" => Self::collect_reserved(member, source, message),
                _ => {}
            }
        }
    }

    fn wire_field(
        node: Node,
        source: &[u8],
        cardinality: ProtoCardinality,
        map_key: Option<CompactStr>,
        oneof: Option<CompactStr>,
    ) -> Option<ProtoWireField> {
        let value_type = Self::find_child_text(node, "type", source)?;
        let name = Self::find_child_text(node, "identifier", source)?;
        let number_node = Self::find_child_by_kind(node, "field_number")?;
        let number = Self::parse_int_lit(number_node.utf8_text(source).ok()?)?;
        Some(ProtoWireField {
            name,
            number,
            cardinality,
            value_type,
            map_key,
            oneof,
            line: node.start_position().row + 1,
        })
    }

    /// `reserved 2, 15, 9 to 11, 40 to max;` or `reserved "foo", "bar";`.
    fn collect_reserved(node: Node, source: &[u8], message: &mut ProtoWireMessage) {
        let mut cursor = node.walk();
        for part in node.children(&mut cursor) {
            match part.kind() {
                "ranges" => {
                    let mut range_cursor = part.walk();
                    for range in part.children(&mut range_cursor) {
                        if range.kind() != "range" {
                            continue;
                        }
                        let mut bounds = Vec::with_capacity(2);
                        let mut to_max = false;
                        let mut bound_cursor = range.walk();
                        for bound in range.children(&mut bound_cursor) {
                            match bound.kind() {
                                "int_lit" => {
                                    if let Some(v) =
                                        bound.utf8_text(source).ok().and_then(Self::parse_int_lit)
                                    {
                                        bounds.push(v);
                                    }
                                }
                                "max" => to_max = true,
                                _ => {}
                            }
                        }
                        match (bounds.as_slice(), to_max) {
                            ([lo], true) => {
                                message.reserved_ranges.push((*lo, PROTO_MAX_FIELD_NUMBER))
                            }
                            ([lo], false) => message.reserved_ranges.push((*lo, *lo)),
                            ([lo, hi], _) => message.reserved_ranges.push((*lo, *hi)),
                            _ => {}
                        }
                    }
                }
                "reserved_field_names" => {
                    let mut name_cursor = part.walk();
                    for name in part.children(&mut name_cursor) {
                        if name.kind() == "reserved_identifier" {
                            if let Ok(text) = name.utf8_text(source) {
                                let unquoted = text.trim().trim_matches(['"', '\'']);
                                if !unquoted.is_empty() {
                                    message.reserved_names.push(CompactStr::new(unquoted));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn has_token(node: Node, token: &str) -> bool {
        let mut cursor = node.walk();
        let found = node
            .children(&mut cursor)
            .any(|c| !c.is_named() && c.kind() == token);
        found
    }

    /// Protobuf `intLit`: decimal, `0`-prefixed octal or `0x` hex.
    fn parse_int_lit(text: &str) -> Option<u32> {
        let text = text.trim();
        if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            u32::from_str_radix(hex, 16).ok()
        } else if text.len() > 1 && text.starts_with('0') {
            u32::from_str_radix(&text[1..], 8).ok()
        } else {
            text.parse().ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_proto::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    /// `import "other.proto";` must be recorded as a file-level dependency,
    /// attributed to every node declared in the file — any of them could
    /// legitimately reference a type declared in the imported file (a
    /// message field typed `google.type.Money`, say).
    #[test]
    fn proto_import_is_recorded_as_a_dependency_on_every_node() {
        let proto = r#"
syntax = "proto3";
package hipstershop;

import "google/protobuf/money.proto";
import public "other/types.proto";

service CartService {
    rpc GetCart(GetCartRequest) returns (Cart) {}
}

message Cart {
    string user_id = 1;
}
"#;
        let mut p = parser();
        let tree = p.parse(proto, None).expect("parse");
        let (nodes, relations) = ProtoExtractor::extract_with_relations(
            Path::new("protos/demo.proto"),
            proto,
            0,
            true,
            &tree,
        );
        assert_eq!(nodes.len(), 3, "CartService, CartService.GetCart, Cart");

        let deps: Vec<&str> = relations
            .dependencies
            .iter()
            .map(|(_, d)| d.as_str())
            .collect();
        assert!(deps.contains(&"google/protobuf/money.proto"));
        assert!(deps.contains(&"other/types.proto"));

        // One edge per import, attributed to the file's own first declared
        // node — not every node (see this function's own doc comment for
        // why: reconcile_edges doesn't dedup Imports edges across different
        // importer_ids, so attributing to every node fanned out into
        // find_dependents false positives for nodes that never actually
        // referenced the import).
        assert_eq!(relations.dependencies.len(), 2);
        assert!(relations.dependencies.iter().all(|(idx, _)| *idx == 0));
    }

    /// A `.proto` file with no imports at all must record none — not an
    /// empty-string placeholder or a fabricated dependency.
    #[test]
    fn proto_with_no_imports_records_no_dependencies() {
        let proto = r#"
syntax = "proto3";
package hipstershop;

message Empty {}
"#;
        let mut p = parser();
        let tree = p.parse(proto, None).expect("parse");
        let (_, relations) = ProtoExtractor::extract_with_relations(
            Path::new("protos/demo.proto"),
            proto,
            0,
            true,
            &tree,
        );
        assert!(relations.dependencies.is_empty());
    }

    /// The protobuf grammar allows single- or double-quoted string literals
    /// for an import path. Only stripping double quotes left a
    /// single-quoted import's dependency key as the literal `'other.proto'`,
    /// quotes included — never matching a real file path.
    #[test]
    fn proto_import_with_single_quotes_is_unquoted_correctly() {
        let proto = "syntax = \"proto3\";\n\nimport 'other.proto';\n\nmessage M {}\n";
        let mut p = parser();
        let tree = p.parse(proto, None).expect("parse");
        let (_, relations) =
            ProtoExtractor::extract_with_relations(Path::new("demo.proto"), proto, 0, true, &tree);
        assert_eq!(relations.dependencies.len(), 1);
        assert_eq!(relations.dependencies[0].1.as_str(), "other.proto");
    }

    #[test]
    fn test_proto_extraction() {
        let proto = r#"
syntax = "proto3";
package auth.v1;

service AuthService {
    rpc AuthenticateUser (AuthRequest) returns (AuthResponse);
}

message AuthRequest {
    string username = 1;
}
"#;
        let nodes = ProtoExtractor::extract(Path::new("proto/auth.proto"), proto, 0);
        assert_eq!(nodes.len(), 3);
        assert!(nodes.iter().any(|n| n.name == "AuthService"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser"));
        assert!(nodes.iter().any(|n| n.name == "AuthRequest"));

        let svc = nodes.iter().find(|n| n.name == "AuthService").unwrap();
        assert_eq!(svc.line_start, 5);
        assert_eq!(svc.package, "auth.v1");

        let rpc = nodes
            .iter()
            .find(|n| n.name == "AuthService.AuthenticateUser")
            .unwrap();
        assert_eq!(rpc.line_start, 6);
        assert_eq!(rpc.package, "auth.v1");

        let msg = nodes.iter().find(|n| n.name == "AuthRequest").unwrap();
        assert_eq!(msg.line_start, 9);
        assert_eq!(msg.package, "auth.v1");
    }

    #[test]
    fn test_proto_extraction_non_canonical_fqcn_projection() {
        let proto = r#"
syntax = "proto3";
package auth.v1;

service AuthService {
    rpc AuthenticateUser (AuthRequest) returns (AuthResponse);
}
"#;
        let nodes =
            ProtoExtractor::extract_with_config(Path::new("proto/auth.proto"), proto, 0, false);
        assert!(nodes.iter().any(|n| n.name == "AuthenticateUser"));
        assert!(!nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser"));
    }

    #[test]
    fn test_proto_nested_options_and_braces_edge_case() {
        let proto = r#"syntax = "proto3";
package api.v1;

service FirstService {
    rpc GetItem (GetItemRequest) returns (Item) {
        option (google.api.http) = {
            get: "/v1/items/{id}"
        };
    }
}

service SecondService {
    rpc ListItems (ListItemsRequest) returns (ListItemsResponse);
}

message OuterMessage {
    message InnerMessage {
        string token = 1;
    }
    InnerMessage inner = 1;
}
"#;
        let nodes = ProtoExtractor::extract(Path::new("proto/items.proto"), proto, 0);
        assert_eq!(nodes.len(), 6);

        assert!(nodes.iter().any(|n| n.name == "FirstService"));
        assert!(nodes.iter().any(|n| n.name == "FirstService.GetItem"));
        // SecondService methods must NOT be assigned to FirstService!
        assert!(nodes.iter().any(|n| n.name == "SecondService"));
        assert!(nodes.iter().any(|n| n.name == "SecondService.ListItems"));
        assert!(!nodes.iter().any(|n| n.name == "FirstService.ListItems"));

        // Nested message support
        assert!(nodes.iter().any(|n| n.name == "OuterMessage"));
        assert!(nodes.iter().any(|n| n.name == "InnerMessage"));
    }

    // --- Wire-format schema (4.6b) -------------------------------------------

    fn schema(src: &str) -> ProtoWireSchema {
        ProtoExtractor::wire_schema(src).expect("wire schema")
    }

    #[test]
    fn wire_schema_collects_fields_oneof_map_nested_and_reserved() {
        let src = r#"syntax = "proto3";
package demo.v1;

enum Color { COLOR_UNSPECIFIED = 0; RED = 1; }

message Outer {
    reserved 5, 9 to 11, 40 to max;
    reserved "legacy", "old_name";
    optional string id = 1;
    repeated int64 ids = 0x2;
    map<string, Inner> by_name = 3;
    oneof choice {
        Color color = 4;
        bytes raw = 012;
    }
    message Inner {
        double score = 1;
    }
}
"#;
        let s = schema(src);
        assert_eq!(s.enum_names, vec![CompactStr::new("Color")]);
        let paths: Vec<&str> = s.messages.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["Outer", "Outer.Inner"]);

        let outer = &s.messages[0];
        let fields: Vec<(&str, u32, ProtoCardinality)> = outer
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.number, f.cardinality))
            .collect();
        assert_eq!(
            fields,
            vec![
                ("id", 1, ProtoCardinality::Singular),
                ("ids", 2, ProtoCardinality::Repeated),
                ("by_name", 3, ProtoCardinality::Map),
                ("color", 4, ProtoCardinality::Singular),
                ("raw", 10, ProtoCardinality::Singular), // octal 012
            ]
        );
        assert_eq!(outer.fields[2].map_key.as_deref(), Some("string"));
        assert_eq!(outer.fields[3].oneof.as_deref(), Some("choice"));
        assert_eq!(
            outer.reserved_ranges,
            vec![(5, 5), (9, 11), (40, PROTO_MAX_FIELD_NUMBER)]
        );
        assert!(outer.is_number_reserved(10));
        assert!(outer.is_number_reserved(1_000));
        assert!(!outer.is_number_reserved(12));
        assert!(outer.is_name_reserved("legacy"));
        assert!(outer.is_name_reserved("old_name"));
    }

    #[test]
    fn wire_schema_refuses_a_file_with_syntax_errors() {
        let err =
            ProtoExtractor::wire_schema("syntax = \"proto3\";\nmessage M {\n  int32 a = ;\n}\n")
                .expect_err("syntax error");
        assert!(matches!(err, WireSchemaError::SyntaxError(_)), "{err:?}");
    }

    /// The compatibility table, both ways: every "compatible" pair listed in
    /// `wire_types_compatible`'s docs is accepted and a representative
    /// incompatible pair from each wire type is refused.
    #[test]
    fn wire_type_compatibility_table() {
        let enums = [CompactStr::new("Color")];
        let ok = |a: &str, b: &str| wire_types_compatible(a, &enums, b, &enums);
        for (a, b) in [
            ("int32", "int64"),
            ("int32", "uint32"),
            ("uint64", "bool"),
            ("int32", "Color"),
            ("sint32", "sint64"),
            ("fixed32", "sfixed32"),
            ("fixed64", "sfixed64"),
            ("string", "bytes"),
            ("bytes", "foo.Bar"),
            (".demo.v1.Bar", "Bar"),
        ] {
            assert!(ok(a, b), "{a} -> {b} must be compatible");
            assert!(ok(b, a), "{b} -> {a} must be compatible");
        }
        for (a, b) in [
            ("int32", "sint32"),
            ("int32", "fixed32"),
            ("fixed32", "float"),
            ("fixed64", "double"),
            ("float", "double"),
            ("int64", "string"),
            ("string", "Bar"),
            ("Bar", "Baz"),
            ("Color", "Bar"),
        ] {
            assert!(!ok(a, b), "{a} -> {b} must be incompatible");
        }
    }

    #[test]
    fn wire_diff_applies_the_three_rules() {
        let old = schema(
            "syntax = \"proto3\";\nmessage M {\n  reserved 7;\n  int32 a = 1;\n  string b = 2;\n  int32 c = 3;\n  int32 d = 4;\n  repeated string e = 5;\n  int32 f = 6;\n}\n",
        );
        let new = schema(
            "syntax = \"proto3\";\nmessage M {\n  reserved 4;\n  reserved \"f\";\n  int64 a = 1;\n  string renamed = 2;\n  sint32 c = 3;\n  string e = 5;\n  int32 g = 7;\n}\n",
        );
        let diff = diff_wire_schemas(&old, &new);
        let found: Vec<(WireRule, u32)> =
            diff.breaking.iter().map(|c| (c.rule, c.number)).collect();
        assert_eq!(
            found,
            vec![
                (WireRule::FieldNumberReused, 2),
                (WireRule::IncompatibleType, 3),
                (WireRule::IncompatibleType, 5),
                (WireRule::FieldNumberReused, 7),
            ],
            "{diff:#?}"
        );
        // a: int32 -> int64 is compatible; d: deleted but number reserved;
        // f: deleted but name reserved.
        assert!(diff.removed_messages.is_empty());
    }

    #[test]
    fn wire_diff_reports_a_deletion_without_reserved_and_removed_messages() {
        let old = schema(
            "syntax = \"proto3\";\nmessage M { int32 a = 1; int32 b = 2; }\nmessage Gone {}\n",
        );
        let new = schema("syntax = \"proto3\";\nmessage M { int32 a = 1; }\n");
        let diff = diff_wire_schemas(&old, &new);
        assert_eq!(diff.breaking.len(), 1);
        assert_eq!(diff.breaking[0].rule, WireRule::DeletedWithoutReserved);
        assert_eq!(diff.breaking[0].number, 2);
        assert_eq!(diff.removed_messages, vec!["Gone".to_string()]);
    }
}
