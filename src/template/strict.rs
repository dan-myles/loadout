use minijinja::Value;
use minijinja::machinery::Span;
use minijinja::machinery::ast::{self, CallArg, Expr, Spanned, Stmt};

/// The filter `rewrite` wraps around every value a template computes. It fails on an undefined
/// value with `Undefined`, so no builtin, container, comparison, or assignment can consume one.
pub(super) const GUARD: &str = "__loadout_defined";

const ESCAPE_TESTS: [&str; 2] = ["defined", "undefined"];
const ESCAPE_FILTERS: [&str; 2] = ["default", "d"];

#[derive(Debug, thiserror::Error)]
#[error("`{expression}` is undefined")]
pub(super) struct Undefined {
    pub(super) expression: String,
    pub(super) lookup: bool,
    pub(super) line: Option<usize>,
}

pub(super) fn guard(
    value: &Value,
    expression: &str,
    lookup: bool,
    line: usize,
) -> Result<Value, minijinja::Error> {
    if value.is_undefined() {
        let undefined = Undefined {
            expression: expression.to_owned(),
            lookup,
            line: Some(line),
        };
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::UndefinedError,
            undefined.to_string(),
        )
        .with_source(undefined));
    }
    Ok(value.clone())
}

/// Rebuilds a parsed template with a guard on every lookup, filter result, and call result that
/// computes a value. What stays unguarded is only what never yields a value to a consumer:
/// - the direct operand of `is defined`, `is undefined`, `default`, and `d`, the explicit ways to
///   handle a missing value (the operand's own receiver and arguments are still guarded);
/// - a name being assigned (`set`, `for`, `with` targets);
/// - a called function's name and a called method's attribute (its receiver is guarded);
/// - the filters chained in a `{% filter %}` or `{% set %}` block header, which have no operand.
///
/// Guards run only when their expression does, so a branch that isn't taken needs nothing.
pub(super) fn rewrite<'a>(source: &'a str, template: &Stmt<'a>) -> Stmt<'a> {
    Rewriter { source }.stmt(template)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Value,
    Escaped,
    Callee,
    Target,
    FilterBlock,
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
            Stmt::ForLoop(node) => Stmt::ForLoop(Spanned::new(
                ast::ForLoop {
                    target: self.expr(&node.target, Target),
                    iter: self.expr(&node.iter, Value),
                    filter_expr: self.optional(node.filter_expr.as_ref(), Value),
                    recursive: node.recursive,
                    body: self.stmts(&node.body),
                    else_body: self.stmts(&node.else_body),
                },
                node.span(),
            )),
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
            Stmt::Set(node) => Stmt::Set(Spanned::new(
                ast::Set {
                    target: self.expr(&node.target, Target),
                    expr: self.expr(&node.expr, Value),
                },
                node.span(),
            )),
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
        Spanned::new(
            ast::Call {
                expr: self.expr(&node.expr, Position::Callee),
                args: self.args(&node.args, Position::Value),
            },
            node.span(),
        )
    }

    fn expr(&self, expr: &Expr<'a>, position: Position) -> Expr<'a> {
        use Position::{Callee, Escaped, FilterBlock, Target, Value};
        // Targets are names being bound, not values being read; they stay exactly as parsed.
        let inner = if position == Target { Target } else { Value };
        match expr {
            Expr::Var(node) => {
                let var = Expr::Var(Spanned::new(ast::Var { id: node.id }, node.span()));
                match position {
                    Escaped | Callee | Target => var,
                    Value | FilterBlock => self.guarded(var, node.span(), true),
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
                        expr: self.expr(&node.expr, inner),
                        name: node.name,
                    },
                    node.span(),
                ));
                match position {
                    Escaped | Callee | Target => attr,
                    Value | FilterBlock => self.guarded(attr, node.span(), true),
                }
            }
            Expr::GetItem(node) => {
                let item = Expr::GetItem(Spanned::new(
                    ast::GetItem {
                        expr: self.expr(&node.expr, inner),
                        subscript_expr: self.expr(&node.subscript_expr, inner),
                    },
                    node.span(),
                ));
                match position {
                    Escaped | Target => item,
                    Value | Callee | FilterBlock => self.guarded(item, node.span(), true),
                }
            }
            Expr::Filter(node) => {
                let operand = if position == FilterBlock {
                    FilterBlock
                } else if ESCAPE_FILTERS.contains(&node.name) {
                    Escaped
                } else {
                    Value
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
                    Escaped | FilterBlock => filter,
                    Value | Callee | Target => self.guarded(filter, span, false),
                }
            }
            Expr::Call(node) => {
                let call = Expr::Call(self.call(node));
                match position {
                    Escaped => call,
                    Value | Callee | Target | FilterBlock => self.guarded(call, node.span(), false),
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
            Expr::IfExpr(node) => Expr::IfExpr(Spanned::new(
                ast::IfExpr {
                    test_expr: self.expr(&node.test_expr, inner),
                    true_expr: self.expr(&node.true_expr, inner),
                    false_expr: self.optional(node.false_expr.as_ref(), inner),
                },
                node.span(),
            )),
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

    fn guarded(&self, expr: Expr<'a>, span: Span, lookup: bool) -> Expr<'a> {
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
                    CallArg::Pos(constant(Value::from(lookup))),
                    CallArg::Pos(constant(Value::from(span.start_line))),
                ],
            },
            span,
        ))
    }
}
