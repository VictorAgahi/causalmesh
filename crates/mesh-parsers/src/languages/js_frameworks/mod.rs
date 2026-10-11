//! Framework and library detectors for JavaScript / TypeScript.
//!
//! The extractor (`typescript.rs`) walks the tree; at each call, `new` and decorator
//! it asks this module whether the site is a broker interaction. A detector is a row
//! of a rule table (`messaging.rs`): the library modules the file must load, the
//! method, constructor or decorator, what the site does ([`Role`]), and how to read
//! the topic / queue / subject off its arguments. Gating on the file's own imports
//! and on the receiver being a client of that library is what lets a generic
//! `.publish(...)` mean "Redis" in one file and nothing in another.
//!
//! The per-file context (modules, constants, clients) is built on the first call
//! site a rule could match, so a file with none pays nothing.

mod file_routes;
mod http;
mod messaging;
pub(crate) mod scope;

pub(crate) use http::{decorator_route, detect_routes, DecoratorRoute, RouteFact};
use mesh_core::NodeKind;
use scope::{is_shadowed, receiver_key, BlockCache, ConstTable, ModuleBindings, Names, Receivers};
use std::cell::OnceCell;
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

/// The facts a file states about itself.
#[derive(Debug)]
pub(crate) struct Built {
    pub bindings: ModuleBindings,
    pub consts: ConstTable,
    pub receivers: Receivers,
    pub scopes: BlockCache,
}

/// Lazily built per-file context.
pub(crate) struct FrameworkCtx<'t> {
    root: Node<'t>,
    source: &'t [u8],
    built: OnceCell<Built>,
    relevant: OnceCell<bool>,
    http_relevant: OnceCell<bool>,
    mounts: OnceCell<http::Mounts>,
    prefix_cache: http::PrefixCache,
}

impl<'t> FrameworkCtx<'t> {
    pub(crate) fn new(root: Node<'t>, source: &'t [u8]) -> Self {
        Self {
            root,
            source,
            built: OnceCell::new(),
            relevant: OnceCell::new(),
            http_relevant: OnceCell::new(),
            mounts: OnceCell::new(),
            prefix_cache: http::PrefixCache::default(),
        }
    }

    /// The memo of the prefixes computed per router (see `http::mounted_prefixes`).
    fn prefix_cache(&self) -> &http::PrefixCache {
        &self.prefix_cache
    }

    /// Whether the file could declare an HTTP route at all (see [`http::mentions_http`]).
    fn http_relevant(&self) -> bool {
        *self
            .http_relevant
            .get_or_init(|| std::str::from_utf8(self.source).is_ok_and(http::mentions_http))
    }

    /// The in-file router mounts, computed on first use.
    fn mounts(&self, names: &Names) -> &http::Mounts {
        self.mounts
            .get_or_init(|| http::collect_mounts(self.root, self.source, names))
    }

    /// Whether the file could contain a broker interaction at all: it names a
    /// library, a decorator or a client-shaped receiver some rule knows. A file
    /// that does not is never analysed further, so ordinary code pays one text scan.
    fn relevant(&self) -> bool {
        *self
            .relevant
            .get_or_init(|| std::str::from_utf8(self.source).is_ok_and(messaging::mentions_broker))
    }

    fn built(&self) -> &Built {
        self.built.get_or_init(|| {
            let bindings = ModuleBindings::collect(self.root, self.source);
            let consts = ConstTable::collect(self.root, self.source, &bindings);
            let families: Vec<&'static [&'static str]> = messaging::FAMILIES
                .iter()
                .chain(http::FAMILIES.iter())
                .copied()
                .collect();
            let receivers =
                Receivers::collect(self.root, self.source, &bindings, &consts, &families);
            Built {
                bindings,
                consts,
                receivers,
                scopes: BlockCache::default(),
            }
        })
    }
}

/// One call site, as the extractors of a rule see it.
pub(crate) struct Site<'a, 't> {
    /// `a.b.method` of `a.b.method(...)`, the constructor expression of `new X(...)`,
    /// or the decorator's function.
    pub callee: Node<'t>,
    pub args: Vec<Node<'t>>,
    pub names: &'a Names<'a>,
    pub fx: &'a Built,
    pub source: &'a [u8],
}

impl Site<'_, '_> {
    /// The name the call's receiver is known by (`pub`, `redis`), if it has one.
    pub(crate) fn receiver_key(&self) -> Option<String> {
        receiver_key(self.callee.child_by_field_name("object")?, self.source)
    }
}

pub(crate) type Extract = for<'a, 't> fn(&Site<'a, 't>) -> Vec<String>;
pub(crate) type When = for<'a, 't> fn(&Site<'a, 't>) -> bool;

/// A `receiver.method(...)` rule.
pub(crate) struct CallRule {
    /// Modules, one of which the file must load; empty means not gated. When the file
    /// builds a client of that library the receiver must be one (see [`Receivers`]).
    pub libs: &'static [&'static str],
    pub method: &'static str,
    pub role: Role,
    pub kind: NodeKind,
    pub signature: &'static str,
    pub extract: Extract,
    /// Extra condition on the call, e.g. the receiver looks like a client.
    pub when: Option<When>,
}

/// A `new Ctor(...)` rule.
pub(crate) struct CtorRule {
    /// Modules the constructor may come from.
    pub libs: &'static [&'static str],
    /// The exported name (`Queue`); `None` for a module's default export (`new Bull('q')`).
    pub name: Option<&'static str>,
    /// Also accept `name` when the file does not import or declare it at all (the
    /// historical `new Queue('x')` detection), but never when it comes from elsewhere.
    pub allow_unbound: bool,
    pub role: Role,
    pub kind: NodeKind,
    pub signature: &'static str,
    pub extract: Extract,
}

/// A `@Decorator(...)` rule.
pub(crate) struct DecoratorRule {
    pub libs: &'static [&'static str],
    /// The exported name of the decorator (`Processor`).
    pub name: &'static str,
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
    if fact.name.trim().is_empty() {
        return;
    }
    if !out
        .iter()
        .any(|f| f.name == fact.name && f.kind == fact.kind && f.role == fact.role)
    {
        out.push(fact);
    }
}

/// Whether the receiver of the call is a client of the rule's library: when the
/// file builds such clients the receiver must be one of them; when it builds none
/// (a client received as an untyped parameter) the import alone gates.
fn receiver_is_client(libs: &[&str], site: &Site) -> bool {
    let Some(family) = libs.first() else {
        return true;
    };
    site.fx
        .receivers
        .is_client(family, site.receiver_key().as_deref().unwrap_or(""))
        .unwrap_or(true)
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
    if !messaging::CALL_RULES.iter().any(|r| r.method == method) || !fx.relevant() {
        return Vec::new();
    }
    let built = fx.built();
    let names = Names {
        source,
        consts: &built.consts,
        bindings: &built.bindings,
        scopes: &built.scopes,
    };
    let site = Site {
        callee,
        args: arguments(call),
        names: &names,
        fx: built,
        source,
    };
    let mut out = Vec::new();
    for rule in messaging::CALL_RULES.iter().filter(|r| r.method == method) {
        if !rule.libs.is_empty() && !built.bindings.imports_any(rule.libs) {
            continue;
        }
        if !receiver_is_client(rule.libs, &site) {
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
/// `None` when a local of the same name (a parameter) hides the import.
fn constructor_origin(
    ctor: Node,
    source: &[u8],
    built: &Built,
) -> Option<(Option<String>, String)> {
    match ctor.kind() {
        "identifier" => {
            let local = ctor.utf8_text(source).ok()?;
            let binding = built.bindings.value_binding(local);
            // Only an imported name or one of the historical bare constructors can
            // match a rule: any other `new Foo()` is left alone, unscanned.
            if binding.is_none() && !matches!(local, "Queue" | "Worker") {
                return None;
            }
            if is_shadowed(ctor, local, source, &built.scopes) {
                return None;
            }
            Some(match binding {
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
            let local = object.utf8_text(source).ok()?;
            let b = built.bindings.value_binding(local)?;
            if is_shadowed(ctor, local, source, &built.scopes) {
                return None;
            }
            Some((Some(b.module.clone()), property.to_string()))
        }
        _ => None,
    }
}

/// Routes the location and exports of a whole file declare (Next, Remix, SvelteKit, Nuxt,
/// Astro file-system routing), with the lines each is anchored to.
pub(crate) fn detect_file_routes(
    path: &std::path::Path,
    fx: &FrameworkCtx,
) -> Vec<(RouteFact, (usize, usize))> {
    if !file_routes::path_may_route(path) {
        return Vec::new();
    }
    let built = fx.built();
    let exports = file_routes::exported_names(fx.root, fx.source);
    if exports.is_empty() {
        return Vec::new();
    }
    let text = std::str::from_utf8(fx.source).unwrap_or("");
    file_routes::file_routes(path, &exports, &built.bindings, &|needle| {
        text.contains(needle)
    })
}

/// Broker interactions at a `new Ctor(...)`.
pub(crate) fn detect_new(node: Node, source: &[u8], fx: &FrameworkCtx) -> Vec<EventFact> {
    let Some(ctor) = node.child_by_field_name("constructor") else {
        return Vec::new();
    };
    // A constructor no rule could name is skipped before the file context is built:
    // an identifier (any imported default may be a queue class) or `ns.Name`.
    if !matches!(ctor.kind(), "identifier" | "member_expression") || !fx.relevant() {
        return Vec::new();
    }
    let built = fx.built();
    let Some((module, exported)) = constructor_origin(ctor, source, built) else {
        return Vec::new();
    };
    let names = Names {
        source,
        consts: &built.consts,
        bindings: &built.bindings,
        scopes: &built.scopes,
    };
    let site = Site {
        callee: ctor,
        args: arguments(node),
        names: &names,
        fx: built,
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
            // Not imported: only the historical bare `new Queue(...)`, and only when
            // the file does not declare a `Queue` of its own.
            (None, Some(name)) => {
                rule.allow_unbound && exported == name && !built.bindings.declares(name)
            }
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

/// Broker interactions declared by a decorator (`@Processor('emails')`).
pub(crate) fn detect_decorator(
    decorator: Node,
    source: &[u8],
    fx: &FrameworkCtx,
) -> Vec<EventFact> {
    let Some(call) = decorator
        .named_child(0)
        .filter(|c| c.kind() == "call_expression")
    else {
        return Vec::new();
    };
    let Some(function) = call.child_by_field_name("function") else {
        return Vec::new();
    };
    let Ok(local) = function.utf8_text(source) else {
        return Vec::new();
    };
    if !messaging::DECORATOR_RULES
        .iter()
        .any(|r| local.ends_with(r.name))
        || !fx.relevant()
    {
        return Vec::new();
    }
    let built = fx.built();
    let Some(binding) = built.bindings.value_binding(local) else {
        return Vec::new();
    };
    let names = Names {
        source,
        consts: &built.consts,
        bindings: &built.bindings,
        scopes: &built.scopes,
    };
    let site = Site {
        callee: function,
        args: arguments(call),
        names: &names,
        fx: built,
        source,
    };
    let mut out = Vec::new();
    for rule in messaging::DECORATOR_RULES {
        if binding.imported != rule.name || !ModuleBindings::module_in(&binding.module, rule.libs) {
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
        .chain(
            messaging::DECORATOR_RULES
                .iter()
                .map(|r| (r.signature, r.role)),
        )
        .find(|(s, _)| *s == signature)
        .map(|(_, role)| role)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn rule_signatures_are_unique() {
        let all: Vec<&str> = messaging::CALL_RULES
            .iter()
            .map(|r| r.signature)
            .chain(messaging::CTOR_RULES.iter().map(|r| r.signature))
            .chain(messaging::DECORATOR_RULES.iter().map(|r| r.signature))
            .collect();
        let unique: HashSet<&str> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len(), "a signature maps to one role only");
    }
}
