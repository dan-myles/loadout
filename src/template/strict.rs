use std::cell::Cell;

use minijinja::machinery::Span;
use minijinja::machinery::ast::{self, CallArg, Expr, Spanned, Stmt};
use minijinja::value::ValueOrKwargs;
use minijinja::{State, Value};

/// The filter `rewrite` wraps around every value a template computes. It fails on an undefined
/// value with `Undefined`, so no builtin, container, comparison, or assignment can consume one.
pub(super) const GUARD: &str = "__loadout_defined";

/// The function a method call named `cycle` is routed through; see `builtins::cycle`.
pub(super) const CYCLE: &str = "__loadout_cycle";

/// The function a method call named `changed` is routed through; see `builtins::changed`.
pub(super) const CHANGED: &str = "__loadout_changed";

/// The function every for-loop body calls first with its `loop`; see `builtins::register_loop`.
pub(super) const LOOP: &str = "__loadout_loop";

/// The function every assignment to a namespace attribute passes its value through; see
/// `builtins::acyclic`.
pub(super) const ACYCLIC: &str = "__loadout_acyclic";

/// The function a target list is unpacked through; see `builtins::unpack`.
pub(super) const UNPACK: &str = "__loadout_unpack";

/// The function that records one target list's unpacked items; see `builtins::store`.
pub(super) const STORE: &str = "__loadout_store";

/// The locals the rewrite binds while it assigns to namespace attributes: the value, the unpacked
/// items of each target list by list number, and the current receiver. Every such statement runs
/// to completion before the next, so they are reused. Templates must not use the prefix.
const VALUE: &str = "__loadout_value";
const PARTS: &str = "__loadout_parts";
const RECEIVER: &str = "__loadout_receiver";

const ESCAPE_TESTS: [&str; 2] = ["defined", "undefined"];
const ESCAPE_FILTERS: [&str; 2] = ["default", "d"];

#[derive(Debug, thiserror::Error)]
#[error("`{expression}` is undefined")]
pub(super) struct Undefined {
    pub(super) expression: String,
    pub(super) lookup: bool,
    pub(super) line: Option<usize>,
}

/// Bits of the guard's `flags` argument.
const LOOKUP: u8 = 1;
const WHOLE: u8 = 2;

/// `WHOLE` is unset only where the value is a receiver: its attributes, items, or methods are read
/// next, and each of those reads is guarded on its own. The value is taken as `ValueOrKwargs`
/// because a plain `Value` argument refuses a keyword-arguments value, which a template can still
/// hold (`loop.cycle(k=1)` returns one) and minijinja passes on like any other; so does every
/// function the rewrite puts where minijinja calls none.
pub(super) fn guard(
    state: &State,
    value: ValueOrKwargs,
    expression: &str,
    flags: u8,
    line: usize,
) -> Result<Value, minijinja::Error> {
    let failure = |message: String, lookup| {
        let undefined = Undefined {
            expression: expression.to_owned(),
            lookup,
            line: Some(line),
        };
        minijinja::Error::new(minijinja::ErrorKind::UndefinedError, message).with_source(undefined)
    };
    if value.is_undefined() {
        return Err(failure(
            format!("`{expression}` is undefined"),
            flags & LOOKUP != 0,
        ));
    }
    if flags & WHOLE != 0
        && let Some(field) = undefined_loop_field(state, &value)
    {
        return Err(failure(
            format!("`{expression}` has no value for `{field}` in a loop of unknown length"),
            false,
        ));
    }
    Ok(value.into_value())
}

// The loop object is the only map the engine builds that lists keys with no value: `length`,
// `revindex`, and `revindex0` are undefined when the loop's iterator has no exact length. Used
// whole (copied by `dict`, `namespace`, or a `**` splat, stored, or passed on), it would carry
// those undefined values into a container. Only the current loop can be one: any alias, copy, or
// container of a loop is made from a whole value, so it was checked while that loop was current.
fn undefined_loop_field(state: &State, value: &Value) -> Option<&'static str> {
    if !is_current_loop(state, value) {
        return None;
    }
    LOOP_KEYS
        .into_iter()
        .find(|key| value.get_attr(key).is_ok_and(|field| field.is_undefined()))
}

pub(super) const LOOP_KEYS: [&str; 9] = [
    "index0",
    "index",
    "length",
    "revindex",
    "revindex0",
    "first",
    "last",
    "depth",
    "depth0",
];

/// Whether `value` is the loop object of the innermost running loop, by object identity with
/// what `loop` names there. Nothing is rendered, iterated, or called.
pub(super) fn is_current_loop(state: &State, value: &Value) -> bool {
    value.as_object().is_some()
        && state
            .lookup("loop")
            .is_some_and(|current| minijinja::tests::is_sameas(value, &current))
}

/// Rebuilds a parsed template with a guard on every lookup, filter result, and call result that
/// computes a value. What stays unguarded is only what never yields a value to a consumer:
/// - the direct operand of `is defined`, `is undefined`, `default`, and `d`, the explicit ways to
///   handle a missing value, also inside a `{% filter %}` or `{% set %}` header chain (the
///   operand's own receiver and arguments are still guarded);
/// - a name being assigned (`set`, `for`, `with` targets);
/// - a called function's name and a called method's attribute (its receiver is guarded);
///
/// The one undefined value a template can still produce is the explicit empty value of an `if`
/// expression without `else`, which reads no missing variable.
///
/// A guarded value is also checked whole, except as a receiver (of an attribute, an item, or a
/// method): a loop object whose length is unknown may be read through, never used as a value.
///
/// Guards run only when their expression does, so a branch that isn't taken needs nothing. A
/// method call named `cycle` goes through `CYCLE`, which refuses an empty argument list
/// instead of letting the loop object divide by zero, and one named `changed` goes through
/// `CHANGED`, which keeps a loop's memory in the render's `State`. Every for-loop body starts by
/// passing its `loop` to `LOOP`.
pub(super) fn rewrite<'a>(source: &'a str, template: &Stmt<'a>) -> Stmt<'a> {
    Rewriter { source }.stmt(template)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Value,
    Receiver,
    Escaped,
    Callee,
    Target,
    FilterBlock,
    ChainEscaped,
}

struct Rewriter<'a> {
    source: &'a str,
}

impl<'a> Rewriter<'a> {
    fn stmts(&self, stmts: &[Stmt<'a>]) -> Vec<Stmt<'a>> {
        stmts.iter().map(|stmt| self.stmt(stmt)).collect()
    }

    fn stmt(&self, stmt: &Stmt<'a>) -> Stmt<'a> {
        use Position::{FilterBlock, Target, Value};
        match stmt {
            Stmt::Template(node) => Stmt::Template(Spanned::new(
                ast::Template {
                    children: self.stmts(&node.children),
                },
                node.span(),
            )),
            Stmt::EmitExpr(node) => Stmt::EmitExpr(Spanned::new(
                ast::EmitExpr {
                    expr: self.expr(&node.expr, Value),
                },
                node.span(),
            )),
            Stmt::EmitRaw(node) => {
                Stmt::EmitRaw(Spanned::new(ast::EmitRaw { raw: node.raw }, node.span()))
            }
            Stmt::ForLoop(node) => {
                // Each iteration first records its `loop` (the raw name, not a guarded value, so
                // an unknown-length loop is never refused here), so that every loop object a
                // template can hold, an alias of an outer or earlier loop included, is known.
                let span = node.span();
                let mut body = vec![Stmt::Do(Spanned::new(
                    ast::Do {
                        call: Spanned::new(
                            ast::Call {
                                expr: variable(LOOP, span),
                                args: vec![CallArg::Pos(variable("loop", span))],
                            },
                            span,
                        ),
                    },
                    span,
                ))];
                body.extend(self.stmts(&node.body));
                Stmt::ForLoop(Spanned::new(
                    ast::ForLoop {
                        target: self.expr(&node.target, Target),
                        iter: self.expr(&node.iter, Value),
                        filter_expr: self.optional(node.filter_expr.as_ref(), Value),
                        recursive: node.recursive,
                        body,
                        else_body: self.stmts(&node.else_body),
                    },
                    span,
                ))
            }
            Stmt::IfCond(node) => Stmt::IfCond(Spanned::new(
                ast::IfCond {
                    expr: self.expr(&node.expr, Value),
                    true_body: self.stmts(&node.true_body),
                    false_body: self.stmts(&node.false_body),
                },
                node.span(),
            )),
            Stmt::WithBlock(node) => Stmt::WithBlock(Spanned::new(
                ast::WithBlock {
                    assignments: node
                        .assignments
                        .iter()
                        .map(|(target, value)| (self.expr(target, Target), self.expr(value, Value)))
                        .collect(),
                    body: self.stmts(&node.body),
                },
                node.span(),
            )),
            Stmt::Set(node) if sets_attribute(&node.target) => {
                let value = self.expr(&node.expr, Value);
                self.assign_attributes(&node.target, node.span(), |temporary| {
                    Stmt::Set(Spanned::new(
                        ast::Set {
                            target: temporary,
                            expr: value,
                        },
                        node.span(),
                    ))
                })
            }
            Stmt::Set(node) => Stmt::Set(Spanned::new(
                ast::Set {
                    target: self.expr(&node.target, Target),
                    expr: self.expr(&node.expr, Value),
                },
                node.span(),
            )),
            Stmt::SetBlock(node) if sets_attribute(&node.target) => {
                let filter = self.optional(node.filter.as_ref(), FilterBlock);
                let body = self.stmts(&node.body);
                self.assign_attributes(&node.target, node.span(), |temporary| {
                    Stmt::SetBlock(Spanned::new(
                        ast::SetBlock {
                            target: temporary,
                            filter,
                            body,
                        },
                        node.span(),
                    ))
                })
            }
            Stmt::SetBlock(node) => Stmt::SetBlock(Spanned::new(
                ast::SetBlock {
                    target: self.expr(&node.target, Target),
                    filter: self.optional(node.filter.as_ref(), FilterBlock),
                    body: self.stmts(&node.body),
                },
                node.span(),
            )),
            Stmt::AutoEscape(node) => Stmt::AutoEscape(Spanned::new(
                ast::AutoEscape {
                    enabled: self.expr(&node.enabled, Value),
                    body: self.stmts(&node.body),
                },
                node.span(),
            )),
            Stmt::FilterBlock(node) => Stmt::FilterBlock(Spanned::new(
                ast::FilterBlock {
                    filter: self.expr(&node.filter, FilterBlock),
                    body: self.stmts(&node.body),
                },
                node.span(),
            )),
            Stmt::Do(node) => Stmt::Do(Spanned::new(
                ast::Do {
                    call: self.call(&node.call),
                },
                node.span(),
            )),
        }
    }

    fn optional(&self, expr: Option<&Expr<'a>>, position: Position) -> Option<Expr<'a>> {
        expr.map(|expr| self.expr(expr, position))
    }

    fn exprs(&self, exprs: &[Expr<'a>], position: Position) -> Vec<Expr<'a>> {
        exprs.iter().map(|expr| self.expr(expr, position)).collect()
    }

    fn args(&self, args: &[CallArg<'a>], position: Position) -> Vec<CallArg<'a>> {
        args.iter()
            .map(|arg| match arg {
                CallArg::Pos(expr) => CallArg::Pos(self.expr(expr, position)),
                CallArg::Kwarg(name, expr) => CallArg::Kwarg(name, self.expr(expr, position)),
                CallArg::PosSplat(expr) => CallArg::PosSplat(self.expr(expr, position)),
                CallArg::KwargSplat(expr) => CallArg::KwargSplat(self.expr(expr, position)),
            })
            .collect()
    }

    fn call(&self, node: &Spanned<ast::Call<'a>>) -> Spanned<ast::Call<'a>> {
        // The loop object's two methods are routed through functions: `cycle`, so that a call
        // with no values cannot divide by zero, and `changed`, so that its memory is kept out of
        // the loop object. The receiver is still evaluated first, then the arguments in order.
        if let Expr::GetAttr(method) = &node.expr
            && let Some(function) = match method.name {
                "cycle" => Some(CYCLE),
                "changed" => Some(CHANGED),
                _ => None,
            }
        {
            let mut args = vec![CallArg::Pos(self.expr(&method.expr, Position::Receiver))];
            args.extend(self.args(&node.args, Position::Value));
            return Spanned::new(
                ast::Call {
                    expr: Expr::Var(Spanned::new(ast::Var { id: function }, method.span())),
                    args,
                },
                node.span(),
            );
        }
        Spanned::new(
            ast::Call {
                expr: self.expr(&node.expr, Position::Callee),
                args: self.args(&node.args, Position::Value),
            },
            node.span(),
        )
    }

    fn expr(&self, expr: &Expr<'a>, position: Position) -> Expr<'a> {
        use Position::{Callee, ChainEscaped, Escaped, FilterBlock, Receiver, Target, Value};
        // Inside a header chain only a filter continues the chain; anything else in an escaped
        // spot is an ordinary escaped operand.
        let position = match (position, expr) {
            (ChainEscaped, Expr::Filter(_)) => ChainEscaped,
            (ChainEscaped, _) => Escaped,
            (position, _) => position,
        };
        // Targets are names being bound, not values being read; they stay exactly as parsed. A
        // receiver is guarded but not checked whole, because only its parts are read next.
        let inner = if position == Target { Target } else { Value };
        let receiver = if position == Target { Target } else { Receiver };
        match expr {
            Expr::Var(node) => {
                let var = Expr::Var(Spanned::new(ast::Var { id: node.id }, node.span()));
                match position {
                    Escaped | ChainEscaped | Callee | Target => var,
                    Value | Receiver | FilterBlock => {
                        self.guarded(var, node.span(), true, position)
                    }
                }
            }
            Expr::Const(node) => Expr::Const(Spanned::new(
                ast::Const {
                    value: node.value.clone(),
                },
                node.span(),
            )),
            Expr::GetAttr(node) => {
                let attr = Expr::GetAttr(Spanned::new(
                    ast::GetAttr {
                        expr: self.expr(&node.expr, receiver),
                        name: node.name,
                    },
                    node.span(),
                ));
                match position {
                    Escaped | ChainEscaped | Callee | Target => attr,
                    Value | Receiver | FilterBlock => {
                        self.guarded(attr, node.span(), true, position)
                    }
                }
            }
            Expr::GetItem(node) => {
                let item = Expr::GetItem(Spanned::new(
                    ast::GetItem {
                        expr: self.expr(&node.expr, receiver),
                        subscript_expr: self.expr(&node.subscript_expr, inner),
                    },
                    node.span(),
                ));
                match position {
                    Escaped | ChainEscaped | Target => item,
                    Value | Receiver | Callee | FilterBlock => {
                        self.guarded(item, node.span(), true, position)
                    }
                }
            }
            Expr::Filter(node) => {
                let in_chain = matches!(position, FilterBlock | ChainEscaped);
                let operand = match (in_chain, ESCAPE_FILTERS.contains(&node.name)) {
                    (true, true) => ChainEscaped,
                    (true, false) => FilterBlock,
                    (false, true) => Escaped,
                    (false, false) => Value,
                };
                let filter = Expr::Filter(Spanned::new(
                    ast::Filter {
                        name: node.name,
                        expr: self.optional(node.expr.as_ref(), operand),
                        args: self.args(&node.args, Value),
                    },
                    node.span(),
                ));
                // A filter's own span starts at its name; the guard names the whole `value | f`.
                let span = match &node.expr {
                    Some(operand) => Span {
                        start_line: operand.span().start_line,
                        start_col: operand.span().start_col,
                        start_offset: operand.span().start_offset,
                        ..node.span()
                    },
                    None => node.span(),
                };
                match position {
                    Escaped | ChainEscaped => filter,
                    Value | Receiver | Callee | Target | FilterBlock => {
                        self.guarded(filter, span, false, position)
                    }
                }
            }
            Expr::Call(node) => {
                let call = Expr::Call(self.call(node));
                match position {
                    Escaped | ChainEscaped => call,
                    Value | Receiver | Callee | Target | FilterBlock => {
                        self.guarded(call, node.span(), false, position)
                    }
                }
            }
            Expr::Test(node) => {
                let operand = if ESCAPE_TESTS.contains(&node.name) {
                    Escaped
                } else {
                    Value
                };
                Expr::Test(Spanned::new(
                    ast::Test {
                        name: node.name,
                        expr: self.expr(&node.expr, operand),
                        args: self.args(&node.args, Value),
                    },
                    node.span(),
                ))
            }
            Expr::Slice(node) => Expr::Slice(Spanned::new(
                ast::Slice {
                    expr: self.expr(&node.expr, inner),
                    start: self.optional(node.start.as_ref(), inner),
                    stop: self.optional(node.stop.as_ref(), inner),
                    step: self.optional(node.step.as_ref(), inner),
                },
                node.span(),
            )),
            Expr::UnaryOp(node) => Expr::UnaryOp(Spanned::new(
                ast::UnaryOp {
                    op: match node.op {
                        ast::UnaryOpKind::Not => ast::UnaryOpKind::Not,
                        ast::UnaryOpKind::Neg => ast::UnaryOpKind::Neg,
                    },
                    expr: self.expr(&node.expr, inner),
                },
                node.span(),
            )),
            Expr::BinOp(node) => Expr::BinOp(Spanned::new(
                ast::BinOp {
                    op: node.op,
                    left: self.expr(&node.left, inner),
                    right: self.expr(&node.right, inner),
                },
                node.span(),
            )),
            Expr::Compare(node) => Expr::Compare(Spanned::new(
                ast::Compare {
                    expr: self.expr(&node.expr, inner),
                    ops: node
                        .ops
                        .iter()
                        .map(|op| ast::CompareOp {
                            op: op.op,
                            expr: self.expr(&op.expr, inner),
                        })
                        .collect(),
                },
                node.span(),
            )),
            Expr::IfExpr(node) => {
                // A chosen branch is what the receiver position reads through.
                let branch = if position == Receiver {
                    Receiver
                } else {
                    inner
                };
                Expr::IfExpr(Spanned::new(
                    ast::IfExpr {
                        test_expr: self.expr(&node.test_expr, inner),
                        true_expr: self.expr(&node.true_expr, branch),
                        false_expr: self.optional(node.false_expr.as_ref(), branch),
                    },
                    node.span(),
                ))
            }
            Expr::List(node) => Expr::List(Spanned::new(
                ast::List {
                    items: self.exprs(&node.items, inner),
                },
                node.span(),
            )),
            Expr::Tuple(node) => Expr::Tuple(Spanned::new(
                ast::Tuple {
                    items: self.exprs(&node.items, inner),
                },
                node.span(),
            )),
            Expr::Map(node) => Expr::Map(Spanned::new(
                ast::Map {
                    keys: self.exprs(&node.keys, inner),
                    values: self.exprs(&node.values, inner),
                },
                node.span(),
            )),
        }
    }

    // `{% set ns.x = v %}` (also unpacked, and as a block) mutates a namespace, the only object a
    // template can change, so it is the only way a reference cycle could form. Codegen evaluates
    // the value once; unpacks a target list once, when it is reached (the top list first, a nested
    // list only after the targets before it are stored); and for each target in order evaluates
    // its receiver and stores into it. This spells out the same steps. Each list's items are
    // unpacked once at that point and kept in `PARTS` under the list's number, so a later target
    // reads that snapshot, never the live value again. `ACYCLIC` checks each part between its
    // receiver and its store.
    fn assign_attributes(
        &self,
        target: &Expr<'a>,
        span: Span,
        bind: impl FnOnce(Expr<'a>) -> Stmt<'a>,
    ) -> Stmt<'a> {
        let mut statements = vec![bind(variable(VALUE, span))];
        let lists = Cell::new(0);
        self.unpack(target, None, &lists, span, &mut statements);
        Stmt::Template(Spanned::new(
            ast::Template {
                children: statements,
            },
            span,
        ))
    }

    /// `part` is the list number and index this target's value comes from; `None` for the value.
    fn unpack(
        &self,
        target: &Expr<'a>,
        part: Option<(usize, usize)>,
        lists: &Cell<usize>,
        span: Span,
        statements: &mut Vec<Stmt<'a>>,
    ) {
        let set = |target: Expr<'a>, expr: Expr<'a>| {
            Stmt::Set(Spanned::new(ast::Set { target, expr }, span))
        };
        match target {
            Expr::GetAttr(attribute) => {
                statements.push(set(
                    variable(RECEIVER, span),
                    self.expr(&attribute.expr, Position::Target),
                ));
                let check = call(
                    ACYCLIC,
                    vec![
                        read(part, span),
                        variable(RECEIVER, span),
                        constant(Value::from(dotted(target)), span),
                    ],
                    span,
                );
                statements.push(set(
                    Expr::GetAttr(Spanned::new(
                        ast::GetAttr {
                            expr: variable(RECEIVER, span),
                            name: attribute.name,
                        },
                        span,
                    )),
                    check,
                ));
            }
            Expr::List(list) => {
                let number = lists.get();
                lists.set(number + 1);
                let known = if number == 0 {
                    constant(Value::from(()), span)
                } else {
                    variable(PARTS, span)
                };
                let items = call(
                    UNPACK,
                    vec![
                        read(part, span),
                        constant(Value::from(list.items.len()), span),
                    ],
                    span,
                );
                statements.push(set(
                    variable(PARTS, span),
                    call(
                        STORE,
                        vec![known, constant(Value::from(number), span), items],
                        span,
                    ),
                ));
                for (index, item) in list.items.iter().enumerate() {
                    self.unpack(item, Some((number, index)), lists, span, statements);
                }
            }
            other => statements.push(set(self.expr(other, Position::Target), read(part, span))),
        }
    }

    fn guarded(&self, expr: Expr<'a>, span: Span, lookup: bool, position: Position) -> Expr<'a> {
        let text = self
            .source
            .get(span.start_offset as usize..span.end_offset as usize)
            .unwrap_or("expression");
        let constant = |value: Value| Expr::Const(Spanned::new(ast::Const { value }, span));
        Expr::Filter(Spanned::new(
            ast::Filter {
                name: GUARD,
                expr: Some(expr),
                args: vec![
                    CallArg::Pos(constant(Value::from(text))),
                    CallArg::Pos(constant(Value::from(
                        u8::from(lookup) * LOOKUP
                            + u8::from(position != Position::Receiver) * WHOLE,
                    ))),
                    CallArg::Pos(constant(Value::from(span.start_line))),
                ],
            },
            span,
        ))
    }
}

fn variable(id: &str, span: Span) -> Expr<'_> {
    Expr::Var(Spanned::new(ast::Var { id }, span))
}

fn sets_attribute(target: &Expr<'_>) -> bool {
    match target {
        Expr::GetAttr(_) => true,
        Expr::List(list) => list.items.iter().any(sets_attribute),
        _ => false,
    }
}

// The dotted name a set target writes, from the parsed names alone: `ns.inner.x`.
fn dotted(target: &Expr<'_>) -> String {
    match target {
        Expr::Var(var) => var.id.to_owned(),
        Expr::GetAttr(attribute) => format!("{}.{}", dotted(&attribute.expr), attribute.name),
        _ => "target".to_owned(),
    }
}

// What one target receives: the bound value, or item `index` of target list `number`'s snapshot.
fn read<'a>(part: Option<(usize, usize)>, span: Span) -> Expr<'a> {
    let Some((number, index)) = part else {
        return variable(VALUE, span);
    };
    let item = |expr, key: usize| {
        Expr::GetItem(Spanned::new(
            ast::GetItem {
                expr,
                subscript_expr: constant(Value::from(key), span),
            },
            span,
        ))
    };
    item(item(variable(PARTS, span), number), index)
}

fn call<'a>(function: &'a str, args: Vec<Expr<'a>>, span: Span) -> Expr<'a> {
    Expr::Call(Spanned::new(
        ast::Call {
            expr: variable(function, span),
            args: args.into_iter().map(CallArg::Pos).collect(),
        },
        span,
    ))
}

fn constant<'a>(value: Value, span: Span) -> Expr<'a> {
    Expr::Const(Spanned::new(ast::Const { value }, span))
}
