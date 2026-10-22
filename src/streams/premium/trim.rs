//! Cuts YouTube's player script down to the code that unscrambles stream
//! URLs, so QuickJS loads a few hundred KB instead of several MB.
//!
//! This follows yt-dlp's solver (github.com/yt-dlp/ejs): the URL scrambler
//! is the top-level function whose body calls `X.y("alr", "yes")`. Parsing
//! happens natively with oxc; the solver's own JavaScript parser is far too
//! slow under QuickJS.

use std::collections::{HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::{ScopeId, Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType, Span};

/// Browser globals the player expects, as yt-dlp's solver stubs them.
const SETUP: &str = r#"if (typeof globalThis.XMLHttpRequest === "undefined") { globalThis.XMLHttpRequest = { prototype: {} }; }
if (typeof URL === "undefined") { globalThis.location = { hash: "", host: "www.youtube.com", hostname: "www.youtube.com", href: "https://www.youtube.com/watch?v=yt-dlp-wins", origin: "https://www.youtube.com", password: "", pathname: "/watch", port: "", protocol: "https:", search: "?v=yt-dlp-wins", username: "" }; } else { globalThis.location = new URL("https://www.youtube.com/watch?v=yt-dlp-wins"); }
if (typeof globalThis.document === "undefined") { globalThis.document = Object.create(null); }
if (typeof globalThis.navigator === "undefined") { globalThis.navigator = Object.create(null); }
if (typeof globalThis.self === "undefined") { globalThis.self = globalThis; }
if (typeof globalThis.window === "undefined") { globalThis.window = globalThis; }
"#;

/// What a top-level statement defines or depends on.
#[derive(Hash, PartialEq, Eq, Clone)]
enum Key {
    Symbol(SymbolId),
    /// A property of the player's namespace parameter, `g.X`.
    Namespace(String),
}

struct Resolver<'s> {
    scoping: &'s Scoping,
    scope: ScopeId,
    namespace: SymbolId,
}

impl Resolver<'_> {
    /// The player-level symbol an identifier refers to, if any.
    fn symbol(&self, ident: &IdentifierReference) -> Option<SymbolId> {
        let reference = ident.reference_id.get()?;
        let symbol = self.scoping.get_reference(reference).symbol_id()?;
        (self.scoping.symbol_scope_id(symbol) == self.scope).then_some(symbol)
    }

    fn namespace_property(&self, member: &StaticMemberExpression) -> Option<Key> {
        match &member.object {
            Expression::Identifier(object) if self.symbol(object) == Some(self.namespace) => {
                Some(Key::Namespace(member.property.name.to_string()))
            }
            _ => None,
        }
    }

    /// The name an assignment defines: `a`, `a` for `a.b.c`, or `g.X`.
    fn assigned(&self, target: &AssignmentTarget) -> Option<Key> {
        let mut object = match target {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                return self.symbol(id).map(Key::Symbol);
            }
            AssignmentTarget::StaticMemberExpression(m) => {
                if let Some(key) = self.namespace_property(m) {
                    return Some(key);
                }
                &m.object
            }
            AssignmentTarget::ComputedMemberExpression(m) => &m.object,
            _ => return None,
        };
        loop {
            match object {
                Expression::Identifier(id) => return self.symbol(id).map(Key::Symbol),
                Expression::StaticMemberExpression(m) => {
                    if let Some(key) = self.namespace_property(m) {
                        return Some(key);
                    }
                    object = &m.object;
                }
                Expression::ComputedMemberExpression(m) => object = &m.object,
                _ => return None,
            }
        }
    }
}

struct References<'r, 's> {
    resolver: &'r Resolver<'s>,
    found: HashSet<Key>,
}

impl<'a> Visit<'a> for References<'_, '_> {
    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        if let Some(symbol) = self.resolver.symbol(it) {
            self.found.insert(Key::Symbol(symbol));
        }
    }

    fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
        // `g.X` depends on whatever defines `g.X`, not on all of `g`.
        if let Some(key) = self.resolver.namespace_property(it) {
            self.found.insert(key);
            return;
        }
        walk::walk_static_member_expression(self, it);
    }
}

fn is_marker(statement: &Statement) -> bool {
    let Statement::ExpressionStatement(s) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &s.expression else {
        return false;
    };
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return false;
    };
    matches!(callee.object, Expression::Identifier(_))
        && call.arguments.len() == 2
        && matches!(
            (&call.arguments[0], &call.arguments[1]),
            (Argument::StringLiteral(a), Argument::StringLiteral(b)) if a.value == "alr" && b.value == "yes"
        )
}

fn is_scrambler(function: &Function) -> bool {
    !function.r#async
        && function
            .body
            .as_ref()
            .is_some_and(|body| body.statements.iter().any(is_marker))
}

fn text(source: &str, span: Span) -> &str {
    &source[span.start as usize..span.end as usize]
}

/// Returns a script that, run as `Function("_result", script)(obj)`, sets
/// `obj.n` and `obj.sig` to functions that unscramble those URL parameters.
pub fn trim(source: &str) -> Result<String, String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::cjs()).parse();
    if let Some(error) = parsed.diagnostics.first() {
        return Err(format!("could not parse the player: {error}"));
    }
    let program = parsed.program;
    let semantic = SemanticBuilder::new().build(&program).semantic;

    // var _yt_player={};(function(g){ ... })(_yt_player);
    let layout = || "unexpected player layout".to_owned();
    let Some(Statement::ExpressionStatement(wrapper)) = program.body.get(1) else {
        return Err(layout());
    };
    let Expression::CallExpression(call) = &wrapper.expression else {
        return Err(layout());
    };
    let Expression::ParenthesizedExpression(parens) = &call.callee else {
        return Err(layout());
    };
    let Expression::FunctionExpression(function) = &parens.expression else {
        return Err(layout());
    };
    let body = function.body.as_ref().ok_or_else(layout)?;
    let resolver = Resolver {
        scoping: semantic.scoping(),
        scope: function.scope_id.get().ok_or_else(layout)?,
        namespace: function
            .params
            .items
            .first()
            .and_then(|p| p.pattern.get_binding_identifier())
            .and_then(|id| id.symbol_id.get())
            .ok_or_else(layout)?,
    };

    // The first statement is `var window=this;`, which the stubs replace.
    let statements: Vec<&Statement> = body.statements.iter().skip(1).collect();
    let mut defined_by: HashMap<Key, Vec<usize>> = HashMap::new();
    let mut depends_on: Vec<HashSet<Key>> = Vec::with_capacity(statements.len());
    let mut scramblers: Vec<(usize, Span)> = Vec::new();

    for (index, statement) in statements.iter().enumerate() {
        let mut defines = Vec::new();
        match statement {
            Statement::FunctionDeclaration(f) => {
                if let Some(id) = &f.id {
                    defines.extend(id.symbol_id.get().map(Key::Symbol));
                    if is_scrambler(f) {
                        scramblers.push((index, id.span));
                    }
                }
            }
            Statement::VariableDeclaration(v) => {
                for declarator in &v.declarations {
                    let Some(id) = declarator.id.get_binding_identifier() else {
                        continue;
                    };
                    defines.extend(id.symbol_id.get().map(Key::Symbol));
                    if let Some(Expression::FunctionExpression(f)) = &declarator.init
                        && is_scrambler(f)
                    {
                        scramblers.push((index, id.span));
                    }
                }
            }
            Statement::ExpressionStatement(e) => match &e.expression {
                Expression::AssignmentExpression(a) => {
                    defines.extend(resolver.assigned(&a.left));
                    if a.operator == AssignmentOperator::Assign
                        && let Expression::FunctionExpression(f) = &a.right
                        && is_scrambler(f)
                    {
                        scramblers.push((index, a.left.span()));
                    }
                }
                Expression::StringLiteral(_) => {}
                // yt-dlp's solver drops other top-level calls too.
                _ => {
                    depends_on.push(HashSet::new());
                    continue;
                }
            },
            _ => {}
        }
        let mut references = References {
            resolver: &resolver,
            found: HashSet::new(),
        };
        references.visit_statement(statement);
        for key in defines {
            defined_by.entry(key).or_default().push(index);
        }
        depends_on.push(references.found);
    }
    if scramblers.is_empty() {
        return Err("could not find the URL scrambler in the player".into());
    }

    // Keep everything the scramblers transitively depend on.
    let mut keep = vec![false; statements.len()];
    let mut pending: Vec<usize> = scramblers.iter().map(|(i, _)| *i).collect();
    while let Some(index) = pending.pop() {
        if std::mem::replace(&mut keep[index], true) {
            continue;
        }
        for key in &depends_on[index] {
            for &other in defined_by.get(key).into_iter().flatten() {
                if !keep[other] {
                    pending.push(other);
                }
            }
        }
    }

    let mut out = String::with_capacity(256 * 1024);
    out.push_str(SETUP);
    // `var _yt_player={};(function(g){`
    out.push_str(&source[program.body[0].span().start as usize..body.span.start as usize + 1]);
    out.push('\n');
    for (statement, _) in statements.iter().zip(&keep).filter(|(_, k)| **k) {
        out.push_str(text(source, statement.span()));
        out.push('\n');
    }
    let solvers: Vec<String> = scramblers
        .iter()
        .map(|(_, span)| solver(text(source, *span)))
        .collect();
    out.push_str(&format!(
        "_result.n = {};\n_result.sig = {};\n",
        multi_try(&solvers, "n"),
        multi_try(&solvers, "sig")
    ));
    // `})(_yt_player);`
    out.push_str(&source[body.span.end as usize - 1..wrapper.span.end as usize]);
    Ok(out)
}

/// Drives the scrambler as yt-dlp's solver does: build a URL object with the
/// challenge, run its unscrambling method, and read the result back.
fn solver(name: &str) -> String {
    format!(
        r#"(({{sig, n}}) => {{ const url = ({name})("https://youtube.com/watch?v=yt-dlp-wins", "s", sig ? encodeURIComponent(sig) : undefined); url.set("n", n); const proto = Object.getPrototypeOf(url); const keys = Object.keys(proto).concat(Object.getOwnPropertyNames(proto)); for (const key of keys) {{ if (!["constructor", "set", "get", "clone"].includes(key)) {{ url[key](); break; }} }} const s = url.get("s"); return {{ sig: s ? decodeURIComponent(s) : null, n: url.get("n") ?? null }}; }})"#
    )
}

/// Tries every candidate and insists they agree.
fn multi_try(solvers: &[String], field: &str) -> String {
    let candidates: Vec<String> = solvers
        .iter()
        .map(|s| format!("({field}) => {s}({{{field}}}).{field}"))
        .collect();
    format!(
        "(_input) => {{ const _results = new Set(); const errors = []; for (const _try of [{}]) {{ try {{ _results.add(_try(_input)); }} catch (e) {{ errors.push(e); }} }} if (!_results.size) throw `no solutions: ${{errors.join(\", \")}}`; if (_results.size !== 1) throw \"conflicting solutions\"; return _results.values().next().value; }}",
        candidates.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature player with the same shape as YouTube's.
    const PLAYER: &str = r#"var _yt_player={};(function(g){var window=this;
var helper = function(x) { return x.split("").reverse().join(""); };
var unused = function() { return 42; };
function Url(base) { this.params = {}; }
Url.prototype.set = function(k, v) { this.params[k] = v; };
Url.prototype.get = function(k) { return this.params[k]; };
Url.prototype.clone = function() { return this; };
Url.prototype.unscramble = function() { if (this.params.n) this.params.n = helper(this.params.n); if (this.params.s) this.params.s = encodeURIComponent(helper(decodeURIComponent(this.params.s))); };
var marker = { mark: function() {} };
g.make = function(base, key, value) { marker.mark("alr", "yes"); var u = new Url(base); if (value !== undefined) u.set(key, value); return u; };
g.other = function() { return unused(); };
init();
})(_yt_player);"#;

    #[test]
    fn keeps_only_what_the_scrambler_needs() {
        let script = trim(PLAYER).unwrap();
        assert!(script.contains("var helper"));
        assert!(script.contains("Url.prototype.unscramble"));
        assert!(script.contains("g.make"));
        assert!(!script.contains("var unused"), "unrelated code is dropped");
        assert!(
            !script.contains("g.other"),
            "unrelated namespace members are dropped"
        );
        assert!(!script.contains("init()"), "top-level calls are dropped");
    }

    #[test]
    fn rejects_players_without_a_scrambler() {
        let player =
            "var _yt_player={};(function(g){var window=this;g.a=function(){};})(_yt_player);";
        assert!(trim(player).is_err());
        assert!(trim("not a player").is_err());
    }
}
