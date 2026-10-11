//! Framework and library detectors for JavaScript / TypeScript.
//!
//! The extractor (`typescript.rs`) walks the tree; at each call and `new` it asks
//! this module whether the site is a broker interaction. A detector is a row of a
//! rule table (`messaging.rs`): the library modules that must be loaded by the
//! file, the method or constructor, what the site does ([`Role`]), and how to read
//! the topic / queue / subject off its arguments. Gating on the file's own imports
//! is what lets a generic `.publish(...)` mean "Redis" in one file and nothing in
//! another.

mod messaging;
pub(crate) mod scope;

use mesh_core::NodeKind;
use scope::{ConstTable, ModuleBindings, Names};
use tree_sitter::Node;

/// Which side of a topic a call site is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Produce,
    Consume,
}

/// A broker interaction found at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventFact {
    pub name: String,
    pub kind: NodeKind,
    /// Identifies the rule that matched; `wire_ts_events` reads the [`Role`] back
    /// from it (see [`signature_role`]).
    pub signature: &'static str,
    pub role: Role,
}

/// Per-file facts the detectors read: the modules the file loads and the string
/// constants it declares.
#[derive(Debug, Default)]
pub(crate) struct FrameworkCtx {
    pub bindings: ModuleBindings,
    pub consts: ConstTable,
}

impl FrameworkCtx {
    pub(crate) fn new(root: Node, source: &[u8]) -> Self {
        let bindings = ModuleBindings::collect(root, source);
        let consts = ConstTable::collect(root, source, &bindings);
        Self { bindings, consts }
    }

    pub(crate) fn names<'a>(&'a self, source: &'a [u8]) -> Names<'a> {
        Names {
            source,
            consts: &self.consts,
            bindings: &self.bindings,
        }
    }
}

/// One call site, as the extractors of a rule see it.
pub(crate) struct Site<'a, 't> {
    /// `a.b.method` of `a.b.method(...)`, or the constructor expression of `new X(...)`.
    pub callee: Node<'t>,
    pub args: Vec<Node<'t>>,
    pub names: &'a Names<'a>,
    pub fx: &'a FrameworkCtx,
    pub source: &'a [u8],
}

pub(crate) type Extract = for<'a, 't> fn(&Site<'a, 't>) -> Vec<String>;

/// A `receiver.method(...)` rule.
pub(crate) struct CallRule {
    /// Modules, one of which the file must load; empty means not gated.
    pub libs: &'static [&'static str],
    pub method: &'static str,
    pub role: Role,
    pub kind: NodeKind,
    pub signature: &'static str,
    pub extract: Extract,
    /// Extra condition on the call, e.g. the receiver looks like a client.
    pub when: Option<for<'a, 't> fn(&Site<'a, 't>) -> bool>,
}

/// A `new Ctor(...)` rule.
pub(crate) struct CtorRule {
    /// Modules the constructor may come from.
    pub libs: &'static [&'static str],
    /// The exported name (`Queue`); `None` for a module's default export (`new Bull('q')`).
    pub name: Option<&'static str>,
    /// Also accept `name` when the file does not import it at all (the historical
    /// `new Queue('x')` detection), but never when it is imported from elsewhere.
    pub allow_unbound: bool,
    pub role: Role,
    pub kind: NodeKind,
    pub signature: &'static str,
    pub extract: Extract,
}

fn arguments<'t>(call: Node<'t>) -> Vec<Node<'t>> {
    call.child_by_field_name("arguments")
        .map(|a| {
            a.named_children(&mut a.walk())
                .filter(|n| n.kind() != "comment")
                .collect()
        })
        .unwrap_or_default()
}

fn push_unique(out: &mut Vec<EventFact>, fact: EventFact) {
    if !out
        .iter()
        .any(|f| f.name == fact.name && f.kind == fact.kind && f.role == fact.role)
    {
        out.push(fact);
    }
}

/// Broker interactions at a `receiver.method(...)` call.
pub(crate) fn detect_call(call: Node, source: &[u8], fx: &FrameworkCtx) -> Vec<EventFact> {
    let Some(callee) = call.child_by_field_name("function") else {
        return Vec::new();
    };
    if callee.kind() != "member_expression" {
        return Vec::new();
    }
    let Some(method) = callee
        .child_by_field_name("property")
        .and_then(|p| p.utf8_text(source).ok())
    else {
        return Vec::new();
    };
    let names = fx.names(source);
    let site = Site {
        callee,
        args: arguments(call),
        names: &names,
        fx,
        source,
    };
    let mut out = Vec::new();
    for rule in messaging::CALL_RULES.iter().filter(|r| r.method == method) {
        if !rule.libs.is_empty() && !fx.bindings.imports_any(rule.libs) {
            continue;
        }
        if rule.when.is_some_and(|when| !when(&site)) {
            continue;
        }
        for name in (rule.extract)(&site) {
            push_unique(
                &mut out,
                EventFact {
                    name,
                    kind: rule.kind,
                    signature: rule.signature,
                    role: rule.role,
                },
            );
        }
    }
    out
}

/// Where a constructor expression comes from: the module it was imported from (if
/// any) and the name it is exported under, or its local name when not imported.
fn constructor_origin(
    ctor: Node,
    source: &[u8],
    fx: &FrameworkCtx,
) -> Option<(Option<String>, String)> {
    match ctor.kind() {
        "identifier" => {
            let local = ctor.utf8_text(source).ok()?;
            Some(match fx.bindings.binding(local) {
                Some(b) => (Some(b.module.clone()), b.imported.clone()),
                None => (None, local.to_string()),
            })
        }
        "member_expression" => {
            let object = ctor.child_by_field_name("object")?;
            let property = ctor
                .child_by_field_name("property")?
                .utf8_text(source)
                .ok()?;
            if object.kind() != "identifier" {
                return None;
            }
            let b = fx.bindings.binding(object.utf8_text(source).ok()?)?;
            Some((Some(b.module.clone()), property.to_string()))
        }
        _ => None,
    }
}

/// Broker interactions at a `new Ctor(...)`.
pub(crate) fn detect_new(node: Node, source: &[u8], fx: &FrameworkCtx) -> Vec<EventFact> {
    let Some(ctor) = node.child_by_field_name("constructor") else {
        return Vec::new();
    };
    let Some((module, exported)) = constructor_origin(ctor, source, fx) else {
        return Vec::new();
    };
    let names = fx.names(source);
    let site = Site {
        callee: ctor,
        args: arguments(node),
        names: &names,
        fx,
        source,
    };
    let mut out = Vec::new();
    for rule in messaging::CTOR_RULES {
        let matches = match (&module, rule.name) {
            // Imported: from one of the rule's modules, under the rule's name (or default).
            (Some(m), Some(name)) => ModuleBindings::module_in(m, rule.libs) && exported == name,
            (Some(m), None) => {
                ModuleBindings::module_in(m, rule.libs)
                    && matches!(exported.as_str(), "default" | "*")
            }
            // Not imported: only the historical bare `new Queue(...)`.
            (None, Some(name)) => rule.allow_unbound && exported == name,
            (None, None) => false,
        };
        if !matches {
            continue;
        }
        for name in (rule.extract)(&site) {
            push_unique(
                &mut out,
                EventFact {
                    name,
                    kind: rule.kind,
                    signature: rule.signature,
                    role: rule.role,
                },
            );
        }
    }
    out
}

/// The [`Role`] of the interaction a node's `signature` was recorded under.
pub(crate) fn signature_role(signature: &str) -> Option<Role> {
    messaging::CALL_RULES
        .iter()
        .map(|r| (r.signature, r.role))
        .chain(messaging::CTOR_RULES.iter().map(|r| (r.signature, r.role)))
        .find(|(s, _)| *s == signature)
        .map(|(_, role)| role)
}
