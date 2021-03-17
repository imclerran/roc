use crate::ast::{AssignedField, CommentOrNewline, Def, Expr, Pattern, Spaceable, TypeAnnotation};
use crate::blankspace::{
    space0_after_e, space0_around_ee, space0_before_e, space0_e, spaces_exactly_e,
};
use crate::ident::{lowercase_ident, parse_ident_help, Ident};
use crate::keyword;
use crate::parser::{
    self, backtrackable, optional, sep_by1, sep_by1_e, specialize, specialize_ref, then,
    trailing_sep_by0, word1, word2, EExpr, EInParens, ELambda, EPattern, ERecord, EString, Either,
    If, List, Number, ParseResult, Parser, State, SyntaxError, Type, When,
};
use crate::pattern::loc_closure_param;
use crate::type_annotation;
use bumpalo::collections::Vec;
use bumpalo::Bump;
use roc_module::operator::{BinOp, CalledVia, UnaryOp};
use roc_region::all::{Located, Position, Region};

use crate::parser::Progress::{self, *};

pub fn test_parse_expr<'a>(
    min_indent: u16,
    arena: &'a bumpalo::Bump,
    state: State<'a>,
) -> Result<Located<Expr<'a>>, EExpr<'a>> {
    let parser = space0_before_e(
        move |a, s| parse_expr_help(min_indent, a, s),
        min_indent,
        EExpr::Space,
        EExpr::IndentStart,
    );

    match parser.parse(arena, state) {
        Ok((_, expression, _)) => Ok(expression),
        Err((_, fail, _)) => Err(fail),
    }
}

// public for testing purposes
pub fn expr<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, SyntaxError<'a>> {
    // Recursive parsers must not directly invoke functions which return (impl Parser),
    // as this causes rustc to stack overflow. Thus, parse_expr must be a
    // separate function which recurses by calling itself directly.
    specialize(
        |e, _, _| SyntaxError::Expr(e),
        move |arena, state: State<'a>| {
            parse_expr_help(min_indent, arena, state).map(|(a, b, c)| (a, b.value, c))
        },
    )
}

pub fn expr_help<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, EExpr<'a>> {
    move |arena, state: State<'a>| {
        parse_expr_help(min_indent, arena, state).map(|(a, b, c)| (a, b.value, c))
    }
}

fn loc_expr_in_parens_help<'a>(
    min_indent: u16,
) -> impl Parser<'a, Located<Expr<'a>>, EInParens<'a>> {
    move |arena, state| {
        let (_, loc_expr, state) = loc_expr_in_parens_help_help(min_indent).parse(arena, state)?;

        Ok((
            MadeProgress,
            Located {
                region: loc_expr.region,
                value: Expr::ParensAround(arena.alloc(loc_expr.value)),
            },
            state,
        ))
    }
}

fn loc_expr_in_parens_help_help<'a>(
    min_indent: u16,
) -> impl Parser<'a, Located<Expr<'a>>, EInParens<'a>> {
    between!(
        word1(b'(', EInParens::Open),
        space0_around_ee(
            specialize_ref(EInParens::Expr, move |arena, state| parse_expr_help(
                min_indent, arena, state
            )),
            min_indent,
            EInParens::Space,
            EInParens::IndentOpen,
            EInParens::IndentEnd,
        ),
        word1(b')', EInParens::End)
    )
}

fn loc_expr_in_parens_etc_help<'a>(
    min_indent: u16,
) -> impl Parser<'a, Located<Expr<'a>>, EExpr<'a>> {
    then(
        loc!(and!(
            specialize(EExpr::InParens, loc_expr_in_parens_help(min_indent)),
            and!(
                one_of![record_field_access_chain(), |a, s| Ok((
                    NoProgress,
                    Vec::new_in(a),
                    s
                ))],
                // TODO remove the either
                optional(
                    // There may optionally be function args after the ')'
                    // e.g. ((foo bar) baz)
                    // loc_function_args_help(min_indent),
                    // If there aren't any args, there may be a '=' or ':' after it.
                    //
                    // (It's a syntax error to write e.g. `foo bar =` - so if there
                    // were any args, there is definitely no need to parse '=' or ':'!)
                    //
                    // Also, there may be a '.' for field access (e.g. `(foo).bar`),
                    // but we only want to look for that if there weren't any args,
                    // as if there were any args they'd have consumed it anyway
                    // e.g. in `((foo bar) baz.blah)` the `.blah` will be consumed by the `baz` parser
                    map!(
                        and!(
                            space0_e(min_indent, EExpr::Space, EExpr::IndentEquals),
                            equals_with_indent_help()
                        ),
                        Either::Second
                    )
                )
            )
        )),
        move |arena, state, _progress, parsed| helper_help(arena, state, parsed, min_indent),
    )
}

fn record_field_access_chain<'a>() -> impl Parser<'a, Vec<'a, &'a str>, EExpr<'a>> {
    |arena, state| match record_field_access().parse(arena, state) {
        Ok((_, initial, state)) => {
            let mut accesses = Vec::with_capacity_in(1, arena);

            accesses.push(initial);

            let mut loop_state = state;
            loop {
                match record_field_access().parse(arena, loop_state) {
                    Ok((_, next, state)) => {
                        accesses.push(next);
                        loop_state = state;
                    }
                    Err((MadeProgress, fail, state)) => return Err((MadeProgress, fail, state)),
                    Err((NoProgress, _, state)) => return Ok((MadeProgress, accesses, state)),
                }
            }
        }
        Err((MadeProgress, fail, state)) => Err((MadeProgress, fail, state)),
        Err((NoProgress, _, state)) => {
            Err((NoProgress, EExpr::Access(state.line, state.column), state))
        }
    }
}

fn record_field_access<'a>() -> impl Parser<'a, &'a str, EExpr<'a>> {
    skip_first!(
        word1(b'.', EExpr::Access),
        specialize(|_, r, c| EExpr::Access(r, c), lowercase_ident())
    )
}

type Extras<'a> = Located<(
    Located<Expr<'a>>,
    (
        Vec<'a, &'a str>,
        Option<Either<Vec<'a, Located<Expr<'a>>>, (&'a [CommentOrNewline<'a>], u16)>>,
    ),
)>;

fn helper_help<'a>(
    arena: &'a Bump,
    state: State<'a>,
    loc_expr_with_extras: Extras<'a>,
    min_indent: u16,
) -> ParseResult<'a, Located<Expr<'a>>, EExpr<'a>> {
    // We parse the parenthetical expression *and* the arguments after it
    // in one region, so that (for example) the region for Apply includes its args.
    let (mut loc_expr, (accesses, opt_extras)) = loc_expr_with_extras.value;

    let mut value = loc_expr.value;

    for field in accesses {
        // Wrap the previous answer in the new one, so we end up
        // with a nested Expr. That way, `foo.bar.baz` gets represented
        // in the AST as if it had been written (foo.bar).baz all along.
        value = Expr::Access(arena.alloc(value), field);
    }

    loc_expr = Located {
        region: loc_expr.region,
        value,
    };

    match opt_extras {
        Some(Either::First(loc_args)) => Ok((
            MadeProgress,
            expr_in_parens_then_arguments(arena, loc_expr, loc_args, loc_expr_with_extras.region),
            state,
        )),
        Some(Either::Second((spaces_before_equals, equals_indent))) => {
            // '=' after optional spaces
            expr_in_parens_then_equals_help(
                min_indent,
                loc_expr,
                spaces_before_equals,
                equals_indent,
                loc_expr_with_extras.region.start_col,
            )
            .parse(arena, state)
        }
        None => Ok((MadeProgress, loc_expr, state)),
    }
}

fn expr_in_parens_then_equals_help<'a>(
    min_indent: u16,
    loc_expr: Located<Expr<'a>>,
    spaces_before_equals: &'a [CommentOrNewline],
    equals_indent: u16,
    def_start_col: u16,
) -> impl Parser<'a, Located<Expr<'a>>, EExpr<'a>> {
    move |arena, state: State<'a>| {
        let region = loc_expr.region;

        // Re-parse the Expr as a Pattern.
        let pattern = match expr_to_pattern_help(arena, &loc_expr.value) {
            Ok(valid) => valid,
            Err(_) => {
                return Err((
                    MadeProgress,
                    EExpr::MalformedPattern(state.line, state.column),
                    state,
                ))
            }
        };

        // Make sure we don't discard the spaces - might be comments in there!
        let value = if spaces_before_equals.is_empty() {
            pattern
        } else {
            Pattern::SpaceAfter(arena.alloc(pattern), spaces_before_equals)
        };

        let loc_first_pattern = Located { region, value };

        // Continue parsing the expression as a Def.
        let (_, spaces_after_equals, state) =
            space0_e(min_indent, EExpr::Space, EExpr::IndentDefBody).parse(arena, state)?;

        // Use loc_expr_with_extras because we want to include the opening '(' char.
        let (_, parsed_expr, state) = parse_def_expr_help(
            min_indent,
            def_start_col,
            equals_indent,
            arena,
            state,
            loc_first_pattern,
            spaces_after_equals,
        )?;

        Ok((
            MadeProgress,
            Located {
                value: parsed_expr,
                region,
            },
            state,
        ))
    }
}

fn expr_in_parens_then_arguments<'a>(
    arena: &'a Bump,
    loc_expr: Located<Expr<'a>>,
    loc_args: Vec<'a, Located<Expr<'a>>>,
    region: Region,
) -> Located<Expr<'a>> {
    let mut allocated_args = Vec::with_capacity_in(loc_args.len(), arena);

    for loc_arg in loc_args {
        allocated_args.push(&*arena.alloc(loc_arg));
    }

    Located {
        region,
        value: Expr::Apply(
            arena.alloc(loc_expr),
            allocated_args.into_bump_slice(),
            CalledVia::Space,
        ),
    }
}

fn parse_loc_term_better<'a>(
    min_indent: u16,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Located<Expr<'a>>, EExpr<'a>> {
    one_of!(
        loc_expr_in_parens_etc_help(min_indent),
        loc!(specialize(EExpr::Str, string_literal_help())),
        loc!(specialize(EExpr::Number, positive_number_literal_help())),
        loc!(specialize(EExpr::Lambda, closure_help(min_indent))),
        loc!(record_literal_help(min_indent)),
        loc!(specialize(EExpr::List, list_literal_help(min_indent))),
        // loc!(ident_etc_help(min_indent))
        loc!(map_with_arena!(
            assign_or_destructure_identifier(),
            ident_to_expr
        )),
    )
    .parse(arena, state)
}

fn loc_possibly_negative_or_negated_term<'a>(
    min_indent: u16,
) -> impl Parser<'a, Located<Expr<'a>>, EExpr<'a>> {
    one_of![
        loc!(map_with_arena!(
            // slight complication; a unary minus must be part of the number literal for overflow
            // reasons
            and!(loc!(unary_negate()), |a, s| parse_loc_term_better(
                min_indent, a, s
            )),
            |arena: &'a Bump, (loc_op, loc_expr): (Located<_>, _)| {
                Expr::UnaryOp(
                    arena.alloc(loc_expr),
                    Located::at(loc_op.region, UnaryOp::Negate),
                )
            }
        )),
        // this will parse negative numbers, which the unary negate thing up top doesn't (for now)
        loc!(specialize(EExpr::Number, number_literal_help())),
        loc!(map_with_arena!(
            and!(loc!(word1(b'!', EExpr::Start)), |a, s| {
                parse_loc_term_better(min_indent, a, s)
            }),
            |arena: &'a Bump, (loc_op, loc_expr): (Located<_>, _)| {
                Expr::UnaryOp(
                    arena.alloc(loc_expr),
                    Located::at(loc_op.region, UnaryOp::Not),
                )
            }
        )),
        |arena, state| {
            // TODO use parse_loc_term_better
            parse_loc_term_better(min_indent, arena, state)
        }
    ]
}

fn fail_expr_start_e<'a, T>() -> impl Parser<'a, T, EExpr<'a>>
where
    T: 'a,
{
    |_arena, state: State<'a>| Err((NoProgress, EExpr::Start(state.line, state.column), state))
}

fn unary_negate<'a>() -> impl Parser<'a, (), EExpr<'a>> {
    move |_arena: &'a Bump, state: State<'a>| {
        // a minus is unary iff
        //
        // - it is preceded by whitespace (spaces, newlines, comments)
        // - it is not followed by whitespace
        // - it is not followed by a number literal
        //
        // The last condition is because of overflow, this would overflow
        //
        //      Num.negate 125
        //
        // while
        //
        //      -125
        //
        // is perfectly fine (assuming I8 here). So it is vital the minus is
        // parses as part of the number literal, and not as a unary minus
        let followed_by_whitespace = state
            .bytes
            .get(1)
            .map(|c| c.is_ascii_whitespace() || *c == b'#' || c.is_ascii_digit())
            .unwrap_or(false);

        if state.bytes.starts_with(b"-") && !followed_by_whitespace {
            // the negate is only unary if it is not followed by whitespace
            Ok((
                MadeProgress,
                (),
                State {
                    bytes: &state.bytes[1..],
                    column: state.column + 1,
                    ..state
                },
            ))
        } else {
            // this is not a negated expression
            Err((NoProgress, EExpr::UnaryNot(state.line, state.column), state))
        }
    }
}

fn parse_expr_start<'a>(
    min_indent: u16,
    start: Position,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Located<Expr<'a>>, EExpr<'a>> {
    one_of![
        loc!(specialize(EExpr::If, if_expr_help(min_indent))),
        loc!(specialize(EExpr::When, when::expr_help(min_indent))),
        loc!(specialize(EExpr::Lambda, closure_help(min_indent))),
        loc!(|a, s| parse_expr_operator_chain(min_indent, start, a, s)),
        fail_expr_start_e()
    ]
    .parse(arena, state)
}

fn parse_expr_operator_chain<'a>(
    min_indent: u16,
    start: Position,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Expr<'a>, EExpr<'a>> {
    let (_, expr, state) = loc_possibly_negative_or_negated_term(min_indent).parse(arena, state)?;

    let initial = state;
    let end = state.get_position();

    match space0_e(min_indent, EExpr::Space, EExpr::IndentEnd).parse(arena, state) {
        Err((_, _, state)) => Ok((MadeProgress, expr.value, state)),
        Ok((_, spaces_before_op, state)) => {
            let expr_state = ExprState {
                operators: Vec::new_in(arena),
                arguments: Vec::new_in(arena),
                expr,
                spaces_after: spaces_before_op,
                initial,
                end,
            };

            parse_expr_end(min_indent, start, expr_state, arena, state)
        }
    }
}

#[derive(Debug)]
struct ExprState<'a> {
    operators: Vec<'a, (Located<Expr<'a>>, Located<BinOp>)>,
    arguments: Vec<'a, &'a Located<Expr<'a>>>,
    expr: Located<Expr<'a>>,
    spaces_after: &'a [CommentOrNewline<'a>],
    initial: State<'a>,
    end: Position,
}

impl<'a> ExprState<'a> {
    fn consume_spaces(&mut self, arena: &'a Bump) {
        if !self.spaces_after.is_empty() {
            if let Some(last) = self.arguments.pop() {
                let new = last.value.with_spaces_after(self.spaces_after, last.region);

                self.arguments.push(arena.alloc(new));
            } else {
                let region = self.expr.region;

                let mut value = Expr::Num("");
                std::mem::swap(&mut self.expr.value, &mut value);

                self.expr = arena
                    .alloc(value)
                    .with_spaces_after(self.spaces_after, region);
            };

            self.spaces_after = &[];
        }
    }

    fn validate_assignment_or_backpassing<F>(
        mut self,
        arena: &'a Bump,
        loc_op: Located<BinOp>,
        argument_error: F,
    ) -> Result<Located<Expr<'a>>, EExpr<'a>>
    where
        F: Fn(Region, Row, Col) -> EExpr<'a>,
    {
        if !self.operators.is_empty() {
            // this `=` or `<-` likely occured inline; treat it as an invalid operator
            let opchar = match loc_op.value {
                BinOp::Assignment => arena.alloc([b'=']) as &[_],
                BinOp::Backpassing => arena.alloc([b'<', b'-']) as &[_],
                _ => unreachable!(),
            };

            let fail =
                EExpr::BadOperator(opchar, loc_op.region.start_line, loc_op.region.start_col);

            Err(fail)
        } else if !self.arguments.is_empty() {
            let region = Region::across_all(self.arguments.iter().map(|v| &v.region));

            Err(argument_error(
                region,
                loc_op.region.start_line,
                loc_op.region.start_col,
            ))
        } else {
            self.consume_spaces(arena);
            Ok(to_call(arena, self.arguments, self.expr, &[]))
        }
    }

    fn validate_has_type(
        mut self,
        arena: &'a Bump,
        loc_op: Located<BinOp>,
    ) -> Result<(Located<Expr<'a>>, Vec<'a, &'a Located<Expr<'a>>>), EExpr<'a>> {
        debug_assert_eq!(loc_op.value, BinOp::HasType);

        if !self.operators.is_empty() {
            // this `:` likely occured inline; treat it as an invalid operator
            let opchar = arena.alloc([b':']) as &[_];

            let fail =
                EExpr::BadOperator(opchar, loc_op.region.start_line, loc_op.region.start_col);

            Err(fail)
        } else {
            self.consume_spaces(arena);
            Ok((self.expr, self.arguments))
        }
    }
}

#[allow(clippy::unnecessary_wraps)]
fn parse_expr_final<'a>(
    _min_indent: u16,
    expr_state: ExprState<'a>,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Expr<'a>, EExpr<'a>> {
    let mut expr = to_call(
        arena,
        expr_state.arguments,
        expr_state.expr,
        expr_state.spaces_after,
    );

    for (left_arg, op) in expr_state.operators.into_iter().rev() {
        let region = Region::span_across(&left_arg.region, &expr.region);
        let new = Expr::BinOp(arena.alloc((left_arg, op, expr)));
        expr = Located::at(region, new);
    }

    Ok((MadeProgress, expr.value, state))
}

fn to_call<'a>(
    arena: &'a Bump,
    arguments: Vec<'a, &'a Located<Expr<'a>>>,
    loc_expr1: Located<Expr<'a>>,
    _spaces_before: &'a [CommentOrNewline<'a>],
) -> Located<Expr<'a>> {
    if arguments.is_empty() {
        loc_expr1
    } else {
        let last = arguments.last().map(|x| x.region).unwrap_or_default();
        let region = Region::span_across(&loc_expr1.region, &last);

        let apply = Expr::Apply(
            arena.alloc(loc_expr1),
            arguments.into_bump_slice(),
            CalledVia::Space,
        );

        Located::at(region, apply)
    }
}

fn numeric_negate_expression<'a>(
    arena: &'a Bump,
    state: State<'a>,
    loc_op: Located<BinOp>,
    expr: Located<Expr<'a>>,
    spaces: &'a [CommentOrNewline<'a>],
) -> Located<Expr<'a>> {
    debug_assert_eq!(state.bytes.get(0), Some(&b'-'));
    // for overflow reasons, we must make the unary minus part of the number literal.
    let mut region = expr.region;
    region.start_col -= 1;

    let new_expr = match &expr.value {
        Expr::Num(string) => {
            let new_string =
                unsafe { std::str::from_utf8_unchecked(&state.bytes[..string.len() + 1]) };

            Expr::Num(new_string)
        }
        Expr::Float(string) => {
            let new_string =
                unsafe { std::str::from_utf8_unchecked(&state.bytes[..string.len() + 1]) };

            Expr::Float(new_string)
        }
        Expr::NonBase10Int {
            string,
            base,
            is_negative,
        } => {
            let new_string =
                unsafe { std::str::from_utf8_unchecked(&state.bytes[..string.len() + 1]) };

            Expr::NonBase10Int {
                is_negative: !is_negative,
                string: new_string,
                base: *base,
            }
        }
        _ => Expr::UnaryOp(
            arena.alloc(expr),
            Located::at(loc_op.region, UnaryOp::Negate),
        ),
    };

    let new_loc_expr = Located::at(region, new_expr);

    if spaces.is_empty() {
        new_loc_expr
    } else {
        arena
            .alloc(new_loc_expr.value)
            .with_spaces_before(spaces, new_loc_expr.region)
    }
}

fn append_body_definition<'a>(
    arena: &'a Bump,
    defs: &mut Vec<'a, &'a Located<Def<'a>>>,
    spaces: &'a [CommentOrNewline<'a>],
    loc_pattern: Located<Pattern<'a>>,
    loc_def_body: Located<Expr<'a>>,
) {
    let region = Region::span_across(&loc_pattern.region, &loc_def_body.region);

    if spaces.len() <= 1 {
        let last = defs.pop();
        match last {
            Some(Located {
                value: Def::Annotation(ann_pattern, ann_type),
                ..
            }) => {
                return append_body_definition_help(
                    arena,
                    defs,
                    region,
                    &[],
                    spaces,
                    loc_pattern,
                    loc_def_body,
                    ann_pattern,
                    ann_type,
                );
            }
            Some(Located {
                value: Def::SpaceBefore(Def::Annotation(ann_pattern, ann_type), before_ann_spaces),
                ..
            }) => {
                return append_body_definition_help(
                    arena,
                    defs,
                    region,
                    before_ann_spaces,
                    spaces,
                    loc_pattern,
                    loc_def_body,
                    ann_pattern,
                    ann_type,
                );
            }
            _ => {
                defs.extend(last);
            }
        }
    }

    // the previous and current def can't be joined up
    let mut loc_def = Located::at(
        region,
        Def::Body(arena.alloc(loc_pattern), &*arena.alloc(loc_def_body)),
    );

    if !spaces.is_empty() {
        loc_def = arena
            .alloc(loc_def.value)
            .with_spaces_before(spaces, loc_def.region);
    }

    defs.push(arena.alloc(loc_def));
}

#[allow(clippy::too_many_arguments)]
fn append_body_definition_help<'a>(
    arena: &'a Bump,
    defs: &mut Vec<'a, &'a Located<Def<'a>>>,
    region: Region,
    before_ann_spaces: &'a [CommentOrNewline<'a>],
    before_body_spaces: &'a [CommentOrNewline<'a>],
    loc_pattern_body: Located<Pattern<'a>>,
    loc_def_body: Located<Expr<'a>>,
    loc_pattern_ann: &'a Located<Pattern<'a>>,
    loc_ann: &'a Located<TypeAnnotation<'a>>,
) {
    let comment = match before_body_spaces.get(0) {
        Some(CommentOrNewline::LineComment(s)) => Some(*s),
        Some(CommentOrNewline::DocComment(s)) => Some(*s),
        _ => None,
    };

    let mut loc_def = Located::at(
        region,
        Def::AnnotatedBody {
            ann_pattern: loc_pattern_ann,
            ann_type: loc_ann,
            comment,
            body_pattern: arena.alloc(loc_pattern_body),
            body_expr: &*arena.alloc(loc_def_body),
        },
    );

    if !before_ann_spaces.is_empty() {
        loc_def = arena
            .alloc(loc_def.value)
            .with_spaces_before(before_ann_spaces, loc_def.region);
    }

    defs.push(arena.alloc(loc_def));
}

fn append_annotation_definition<'a>(
    arena: &'a Bump,
    defs: &mut Vec<'a, &'a Located<Def<'a>>>,
    spaces: &'a [CommentOrNewline<'a>],
    loc_pattern: Located<Pattern<'a>>,
    loc_ann: Located<TypeAnnotation<'a>>,
) {
    let region = Region::span_across(&loc_pattern.region, &loc_ann.region);

    // the previous and current def can't be joined up
    match &loc_pattern.value {
        Pattern::Apply(
            Located {
                value: Pattern::GlobalTag(name),
                ..
            },
            alias_arguments,
        ) => append_alias_definition(
            arena,
            defs,
            region,
            spaces,
            Located::at(loc_pattern.region, name),
            alias_arguments,
            loc_ann,
        ),
        Pattern::GlobalTag(name) => append_alias_definition(
            arena,
            defs,
            region,
            spaces,
            Located::at(loc_pattern.region, name),
            &[],
            loc_ann,
        ),
        _ => {
            let mut loc_def = Located::at(region, Def::Annotation(loc_pattern, loc_ann));
            if !spaces.is_empty() {
                loc_def = arena
                    .alloc(loc_def.value)
                    .with_spaces_before(spaces, loc_def.region);
            }

            defs.push(arena.alloc(loc_def));
        }
    }
}

fn append_alias_definition<'a>(
    arena: &'a Bump,
    defs: &mut Vec<'a, &'a Located<Def<'a>>>,
    region: Region,
    spaces: &'a [CommentOrNewline<'a>],
    name: Located<&'a str>,
    pattern_arguments: &'a [Located<Pattern<'a>>],
    loc_ann: Located<TypeAnnotation<'a>>,
) {
    let def = Def::Alias {
        name,
        vars: pattern_arguments,
        ann: loc_ann,
    };
    let mut loc_def = Located::at(region, def);

    if !spaces.is_empty() {
        loc_def = arena
            .alloc(loc_def.value)
            .with_spaces_before(spaces, loc_def.region);
    }

    defs.push(arena.alloc(loc_def));
}

#[derive(Debug)]
struct DefState<'a> {
    defs: Vec<'a, &'a Located<Def<'a>>>,
    spaces_after: &'a [CommentOrNewline<'a>],
}

fn parse_defs_end<'a>(
    start: Position,
    mut def_state: DefState<'a>,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, DefState<'a>, EExpr<'a>> {
    let min_indent = start.col;
    let initial = state;

    let state = match space0_e(min_indent, EExpr::Space, EExpr::IndentStart).parse(arena, state) {
        Err((MadeProgress, _, s)) => {
            return Err((
                MadeProgress,
                EExpr::DefMissingFinalExpr(s.line, s.column),
                s,
            ));
        }
        Ok((_, spaces, state)) => {
            def_state.spaces_after = spaces;
            state
        }
        Err((NoProgress, _, state)) => state,
    };

    match space0_after_e(
        crate::pattern::loc_pattern_help(min_indent),
        min_indent,
        EPattern::Space,
        EPattern::IndentEnd,
    )
    .parse(arena, state)
    {
        Err((_, _, _)) => {
            // a hacky way to get expression-based error messages. TODO fix this
            Ok((NoProgress, def_state, initial))
        }
        Ok((_, loc_pattern, state)) => match operator().parse(arena, state) {
            Ok((_, BinOp::Assignment, state)) => {
                let parse_def_expr = space0_before_e(
                    move |a, s| parse_expr_help(min_indent + 1, a, s),
                    min_indent,
                    EExpr::Space,
                    EExpr::IndentEnd,
                );

                let (_, loc_def_expr, state) = parse_def_expr.parse(arena, state)?;

                append_body_definition(
                    arena,
                    &mut def_state.defs,
                    def_state.spaces_after,
                    loc_pattern,
                    loc_def_expr,
                );

                parse_defs_end(start, def_state, arena, state)
            }
            Ok((_, BinOp::HasType, state)) => {
                let (_, ann_type, state) = specialize(
                    EExpr::Type,
                    space0_before_e(
                        type_annotation::located_help(min_indent + 1),
                        min_indent + 1,
                        Type::TSpace,
                        Type::TIndentStart,
                    ),
                )
                .parse(arena, state)?;

                append_annotation_definition(
                    arena,
                    &mut def_state.defs,
                    def_state.spaces_after,
                    loc_pattern,
                    ann_type,
                );

                parse_defs_end(start, def_state, arena, state)
            }
            _ => Ok((MadeProgress, def_state, initial)),
        },
    }
}

fn parse_defs_expr<'a>(
    start: Position,
    def_state: DefState<'a>,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Expr<'a>, EExpr<'a>> {
    let min_indent = start.col;

    match parse_defs_end(start, def_state, arena, state) {
        Err(bad) => Err(bad),
        Ok((_, def_state, state)) => {
            // this is no def, because there is no `=` or `:`; parse as an expr
            let parse_final_expr = space0_before_e(
                move |a, s| parse_expr_help(min_indent, a, s),
                min_indent,
                EExpr::Space,
                EExpr::IndentEnd,
            );

            match parse_final_expr.parse(arena, state) {
                Err((_, fail, state)) => {
                    return Err((
                        MadeProgress,
                        EExpr::DefMissingFinalExpr2(arena.alloc(fail), state.line, state.column),
                        state,
                    ));
                }
                Ok((_, loc_ret, state)) => {
                    return Ok((
                        MadeProgress,
                        Expr::Defs(def_state.defs.into_bump_slice(), arena.alloc(loc_ret)),
                        state,
                    ));
                }
            }
        }
    }
}

fn parse_expr_operator<'a>(
    min_indent: u16,
    start: Position,
    mut expr_state: ExprState<'a>,
    loc_op: Located<BinOp>,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Expr<'a>, EExpr<'a>> {
    let (_, spaces_after_operator, state) =
        space0_e(min_indent, EExpr::Space, EExpr::IndentEnd).parse(arena, state)?;

    // a `-` is unary if it is preceded by a space and not followed by a space

    let op = loc_op.value;
    let op_start = loc_op.region.start();
    let op_end = loc_op.region.end();
    let new_start = state.get_position();
    match op {
        BinOp::Minus if expr_state.end != op_start && op_end == new_start => {
            // negative terms

            let (_, negated_expr, state) = parse_loc_term_better(min_indent, arena, state)?;
            let new_end = state.get_position();

            let arg = numeric_negate_expression(
                arena,
                expr_state.initial,
                loc_op,
                negated_expr,
                expr_state.spaces_after,
            );

            expr_state.initial = state;

            let (spaces, state) =
                match space0_e(min_indent, EExpr::Space, EExpr::IndentEnd).parse(arena, state) {
                    Err((_, _, state)) => (&[] as &[_], state),
                    Ok((_, spaces, state)) => (spaces, state),
                };

            expr_state.arguments.push(arena.alloc(arg));
            expr_state.spaces_after = spaces;
            expr_state.end = new_end;

            parse_expr_end(min_indent, start, expr_state, arena, state)
        }
        BinOp::Assignment => {
            let expr_region = expr_state.expr.region;
            let indented_more = start.col + 1;

            let call = expr_state
                .validate_assignment_or_backpassing(arena, loc_op, EExpr::ElmStyleFunction)
                .map_err(|fail| (MadeProgress, fail, state))?;

            let (loc_def, state) = {
                match expr_to_pattern_help(arena, &call.value) {
                    Ok(good) => {
                        let (_, mut ann_type, state) =
                            parse_expr_help(indented_more, arena, state)?;

                        // put the spaces from after the operator in front of the call
                        if !spaces_after_operator.is_empty() {
                            ann_type = arena
                                .alloc(ann_type.value)
                                .with_spaces_before(spaces_after_operator, ann_type.region);
                        }

                        let alias_region = Region::span_across(&call.region, &ann_type.region);

                        let alias = Def::Body(
                            arena.alloc(Located::at(expr_region, good)),
                            arena.alloc(ann_type),
                        );

                        (&*arena.alloc(Located::at(alias_region, alias)), state)
                    }
                    Err(_) => {
                        // this `=` likely occured inline; treat it as an invalid operator
                        let fail = EExpr::BadOperator(
                            arena.alloc([b'=']),
                            loc_op.region.start_line,
                            loc_op.region.start_col,
                        );

                        return Err((MadeProgress, fail, state));
                    }
                }
            };

            let def_state = DefState {
                defs: bumpalo::vec![in arena; loc_def],
                spaces_after: &[],
            };

            parse_defs_expr(start, def_state, arena, state)
        }
        BinOp::Backpassing => {
            let expr_region = expr_state.expr.region;
            let indented_more = start.col + 1;

            let call = expr_state
                .validate_assignment_or_backpassing(arena, loc_op, |_, r, c| {
                    EExpr::BadOperator(&[b'<', b'-'], r, c)
                })
                .map_err(|fail| (MadeProgress, fail, state))?;

            let (loc_pattern, loc_body, state) = {
                match expr_to_pattern_help(arena, &call.value) {
                    Ok(good) => {
                        let (_, mut ann_type, state) =
                            parse_expr_help(indented_more, arena, state)?;

                        // put the spaces from after the operator in front of the call
                        if !spaces_after_operator.is_empty() {
                            ann_type = arena
                                .alloc(ann_type.value)
                                .with_spaces_before(spaces_after_operator, ann_type.region);
                        }

                        (Located::at(expr_region, good), ann_type, state)
                    }
                    Err(_) => {
                        // this `=` likely occured inline; treat it as an invalid operator
                        let fail = EExpr::BadOperator(
                            arena.alloc([b'=']),
                            loc_op.region.start_line,
                            loc_op.region.start_col,
                        );

                        return Err((MadeProgress, fail, state));
                    }
                }
            };

            let parse_cont = space0_before_e(
                move |a, s| parse_expr_help(min_indent, a, s),
                min_indent,
                EExpr::Space,
                EExpr::IndentEnd,
            );

            let (_, loc_cont, state) = parse_cont.parse(arena, state)?;

            let ret = Expr::Backpassing(
                arena.alloc([loc_pattern]),
                arena.alloc(loc_body),
                arena.alloc(loc_cont),
            );

            Ok((MadeProgress, ret, state))
        }
        BinOp::HasType => {
            let expr_region = expr_state.expr.region;
            let indented_more = start.col + 1;

            let (expr, arguments) = expr_state
                .validate_has_type(arena, loc_op)
                .map_err(|fail| (MadeProgress, fail, state))?;

            let (loc_def, state) = match &expr.value {
                Expr::GlobalTag(name) => {
                    let mut type_arguments = Vec::with_capacity_in(arguments.len(), arena);

                    for argument in arguments {
                        match expr_to_pattern_help(arena, &argument.value) {
                            Ok(good) => {
                                type_arguments.push(Located::at(argument.region, good));
                            }
                            Err(_) => panic!(),
                        }
                    }

                    let (_, ann_type, state) = specialize(
                        EExpr::Type,
                        space0_before_e(
                            type_annotation::located_help(indented_more),
                            min_indent,
                            Type::TSpace,
                            Type::TIndentStart,
                        ),
                    )
                    .parse(arena, state)?;

                    let alias_region = Region::span_across(&expr.region, &ann_type.region);

                    let alias = Def::Alias {
                        name: Located::at(expr.region, name),
                        vars: type_arguments.into_bump_slice(),
                        ann: ann_type,
                    };

                    (&*arena.alloc(Located::at(alias_region, alias)), state)
                }

                _ => {
                    let call = to_call(arena, arguments, expr, spaces_after_operator);

                    match expr_to_pattern_help(arena, &call.value) {
                        Ok(good) => {
                            let (_, mut ann_type, state) = specialize(
                                EExpr::Type,
                                space0_before_e(
                                    type_annotation::located_help(indented_more),
                                    min_indent,
                                    Type::TSpace,
                                    Type::TIndentStart,
                                ),
                            )
                            .parse(arena, state)?;

                            // put the spaces from after the operator in front of the call
                            if !spaces_after_operator.is_empty() {
                                ann_type = arena
                                    .alloc(ann_type.value)
                                    .with_spaces_before(spaces_after_operator, ann_type.region);
                            }

                            let alias_region = Region::span_across(&call.region, &ann_type.region);

                            let alias = Def::Annotation(Located::at(expr_region, good), ann_type);

                            (&*arena.alloc(Located::at(alias_region, alias)), state)
                        }
                        Err(_) => {
                            // this `:` likely occured inline; treat it as an invalid operator
                            let fail = EExpr::BadOperator(
                                arena.alloc([b':']),
                                loc_op.region.start_line,
                                loc_op.region.start_col,
                            );

                            return Err((MadeProgress, fail, state));
                        }
                    }
                }
            };

            let def_state = DefState {
                defs: bumpalo::vec![in arena; loc_def],
                spaces_after: &[],
            };

            parse_defs_expr(start, def_state, arena, state)
        }
        _ => match loc_possibly_negative_or_negated_term(min_indent).parse(arena, state) {
            Err((MadeProgress, f, s)) => Err((MadeProgress, f, s)),
            Ok((_, mut new_expr, state)) => {
                let new_end = state.get_position();

                expr_state.initial = state;

                // put the spaces from after the operator in front of the new_expr
                if !spaces_after_operator.is_empty() {
                    new_expr = arena
                        .alloc(new_expr.value)
                        .with_spaces_before(spaces_after_operator, new_expr.region);
                }

                match space0_e(min_indent, EExpr::Space, EExpr::IndentEnd).parse(arena, state) {
                    Err((_, _, state)) => {
                        let args = std::mem::replace(&mut expr_state.arguments, Vec::new_in(arena));

                        let call = to_call(arena, args, expr_state.expr, &[]);

                        expr_state.operators.push((call, loc_op));
                        expr_state.expr = new_expr;
                        expr_state.end = new_end;
                        expr_state.spaces_after = &[];

                        parse_expr_final(min_indent, expr_state, arena, state)
                    }
                    Ok((_, spaces, state)) => {
                        let args = std::mem::replace(&mut expr_state.arguments, Vec::new_in(arena));

                        let call = to_call(arena, args, expr_state.expr, spaces);

                        expr_state.operators.push((call, loc_op));
                        expr_state.expr = new_expr;
                        expr_state.end = new_end;
                        expr_state.spaces_after = spaces;

                        // TODO new start?
                        parse_expr_end(min_indent, start, expr_state, arena, state)
                    }
                }
            }
            Err((NoProgress, _, _)) => {
                todo!()
            }
        },
    }
}

fn parse_expr_end<'a>(
    min_indent: u16,
    start: Position,
    mut expr_state: ExprState<'a>,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Expr<'a>, EExpr<'a>> {
    let parser = skip_first!(
        crate::blankspace::check_indent(min_indent, EExpr::IndentEnd),
        move |a, s| parse_loc_term_better(min_indent, a, s)
    );

    match parser.parse(arena, state) {
        Err((MadeProgress, f, s)) => Err((MadeProgress, f, s)),
        Ok((_, mut arg, state)) => {
            let new_end = state.get_position();

            // now that we have `function arg1 ... <spaces> argn`, attach the spaces to the `argn`
            if !expr_state.spaces_after.is_empty() {
                arg = arena
                    .alloc(arg.value)
                    .with_spaces_before(expr_state.spaces_after, arg.region);

                expr_state.spaces_after = &[];
            }
            expr_state.initial = state;

            match space0_e(min_indent, EExpr::Space, EExpr::IndentEnd).parse(arena, state) {
                Err((_, _, state)) => {
                    expr_state.arguments.push(arena.alloc(arg));
                    expr_state.end = new_end;
                    expr_state.spaces_after = &[];

                    parse_expr_final(min_indent, expr_state, arena, state)
                }
                Ok((_, new_spaces, state)) => {
                    expr_state.arguments.push(arena.alloc(arg));
                    expr_state.end = new_end;
                    expr_state.spaces_after = new_spaces;

                    parse_expr_end(min_indent, start, expr_state, arena, state)
                }
            }
        }
        Err((NoProgress, _, _)) => {
            let before_op = state;
            // try an operator
            match loc!(operator()).parse(arena, state) {
                Err((MadeProgress, f, s)) => Err((MadeProgress, f, s)),
                Ok((_, loc_op, state)) => {
                    expr_state.consume_spaces(arena);
                    expr_state.initial = before_op;
                    parse_expr_operator(min_indent, start, expr_state, loc_op, arena, state)
                }
                Err((NoProgress, _, _)) => {
                    // roll back space parsing
                    let state = expr_state.initial;

                    if expr_state.operators.is_empty() {
                        let expr = to_call(
                            arena,
                            expr_state.arguments,
                            expr_state.expr,
                            expr_state.spaces_after,
                        );

                        Ok((MadeProgress, expr.value, state))
                    } else {
                        let mut expr = to_call(
                            arena,
                            expr_state.arguments,
                            expr_state.expr,
                            expr_state.spaces_after,
                        );

                        for (left_arg, op) in expr_state.operators.into_iter().rev() {
                            let region = Region::span_across(&left_arg.region, &expr.region);
                            let new = Expr::BinOp(arena.alloc((left_arg, op, expr)));
                            expr = Located::at(region, new);
                        }

                        Ok((MadeProgress, expr.value, state))
                    }
                }
            }
        }
    }
}

fn parse_expr_help<'a>(
    min_indent: u16,
    arena: &'a Bump,
    state: State<'a>,
) -> ParseResult<'a, Located<Expr<'a>>, EExpr<'a>> {
    let start = state.get_position();
    parse_expr_start(min_indent, start, arena, state)
}

/// If the given Expr would parse the same way as a valid Pattern, convert it.
/// Example: (foo) could be either an Expr::Var("foo") or Pattern::Identifier("foo")
fn expr_to_pattern_help<'a>(arena: &'a Bump, expr: &Expr<'a>) -> Result<Pattern<'a>, ()> {
    match expr {
        Expr::Var { module_name, ident } => {
            if module_name.is_empty() {
                Ok(Pattern::Identifier(ident))
            } else {
                Ok(Pattern::QualifiedIdentifier { module_name, ident })
            }
        }
        Expr::GlobalTag(value) => Ok(Pattern::GlobalTag(value)),
        Expr::PrivateTag(value) => Ok(Pattern::PrivateTag(value)),
        Expr::Apply(loc_val, loc_args, _) => {
            let region = loc_val.region;
            let value = expr_to_pattern_help(arena, &loc_val.value)?;
            let val_pattern = arena.alloc(Located { region, value });

            let mut arg_patterns = Vec::with_capacity_in(loc_args.len(), arena);

            for loc_arg in loc_args.iter() {
                let region = loc_arg.region;
                let value = expr_to_pattern_help(arena, &loc_arg.value)?;

                arg_patterns.push(Located { region, value });
            }

            let pattern = Pattern::Apply(val_pattern, arg_patterns.into_bump_slice());

            Ok(pattern)
        }

        Expr::SpaceBefore(sub_expr, spaces) => Ok(Pattern::SpaceBefore(
            arena.alloc(expr_to_pattern_help(arena, sub_expr)?),
            spaces,
        )),
        Expr::SpaceAfter(sub_expr, spaces) => Ok(Pattern::SpaceAfter(
            arena.alloc(expr_to_pattern_help(arena, sub_expr)?),
            spaces,
        )),

        Expr::ParensAround(sub_expr) | Expr::Nested(sub_expr) => {
            expr_to_pattern_help(arena, sub_expr)
        }

        Expr::Record {
            fields,
            update: None,
            final_comments: _,
        } => {
            let mut loc_patterns = Vec::with_capacity_in(fields.len(), arena);

            for loc_assigned_field in fields.iter() {
                let region = loc_assigned_field.region;
                let value = assigned_expr_field_to_pattern_help(arena, &loc_assigned_field.value)?;

                loc_patterns.push(Located { region, value });
            }

            Ok(Pattern::RecordDestructure(loc_patterns.into_bump_slice()))
        }

        Expr::Float(string) => Ok(Pattern::FloatLiteral(string)),
        Expr::Num(string) => Ok(Pattern::NumLiteral(string)),
        Expr::NonBase10Int {
            string,
            base,
            is_negative,
        } => Ok(Pattern::NonBase10Literal {
            string,
            base: *base,
            is_negative: *is_negative,
        }),
        // These would not have parsed as patterns
        Expr::AccessorFunction(_)
        | Expr::Access(_, _)
        | Expr::List { .. }
        | Expr::Closure(_, _)
        | Expr::Backpassing(_, _, _)
        | Expr::BinOp(_)
        | Expr::Defs(_, _)
        | Expr::If(_, _)
        | Expr::When(_, _)
        | Expr::MalformedClosure
        | Expr::PrecedenceConflict(_, _, _, _)
        | Expr::Record {
            update: Some(_), ..
        }
        | Expr::UnaryOp(_, _) => Err(()),

        Expr::Str(string) => Ok(Pattern::StrLiteral(string.clone())),
        Expr::MalformedIdent(string, _problem) => Ok(Pattern::Malformed(string)),
    }
}

fn assigned_expr_field_to_pattern_help<'a>(
    arena: &'a Bump,
    assigned_field: &AssignedField<'a, Expr<'a>>,
) -> Result<Pattern<'a>, ()> {
    // the assigned fields always store spaces, but this slice is often empty
    Ok(match assigned_field {
        AssignedField::RequiredValue(name, spaces, value) => {
            let pattern = expr_to_pattern_help(arena, &value.value)?;
            let result = arena.alloc(Located {
                region: value.region,
                value: pattern,
            });
            if spaces.is_empty() {
                Pattern::RequiredField(name.value, result)
            } else {
                Pattern::SpaceAfter(
                    arena.alloc(Pattern::RequiredField(name.value, result)),
                    spaces,
                )
            }
        }
        AssignedField::OptionalValue(name, spaces, value) => {
            let result = arena.alloc(Located {
                region: value.region,
                value: value.value.clone(),
            });
            if spaces.is_empty() {
                Pattern::OptionalField(name.value, result)
            } else {
                Pattern::SpaceAfter(
                    arena.alloc(Pattern::OptionalField(name.value, result)),
                    spaces,
                )
            }
        }
        AssignedField::LabelOnly(name) => Pattern::Identifier(name.value),
        AssignedField::SpaceBefore(nested, spaces) => Pattern::SpaceBefore(
            arena.alloc(assigned_expr_field_to_pattern_help(arena, nested)?),
            spaces,
        ),
        AssignedField::SpaceAfter(nested, spaces) => Pattern::SpaceAfter(
            arena.alloc(assigned_expr_field_to_pattern_help(arena, nested)?),
            spaces,
        ),
        AssignedField::Malformed(string) => Pattern::Malformed(string),
    })
}

fn parse_defs_help<'a>(
    min_indent: u16,
) -> impl Parser<'a, Vec<'a, &'a Located<Def<'a>>>, EExpr<'a>> {
    let parse_def = move |arena, state| {
        let (_, (spaces, def), state) = and!(
            backtrackable(space0_e(min_indent, EExpr::Space, EExpr::IndentStart)),
            loc!(def_help(min_indent))
        )
        .parse(arena, state)?;

        let result = if spaces.is_empty() {
            &*arena.alloc(def)
        } else {
            &*arena.alloc(
                arena
                    .alloc(def.value)
                    .with_spaces_before(spaces, def.region),
            )
        };

        Ok((MadeProgress, result, state))
    };

    zero_or_more!(parse_def)
}

/// A definition, consisting of one of these:
///
/// * A type alias using `:`
/// * A pattern followed by '=' and then an expression
/// * A type annotation
/// * A type annotation followed on the next line by a pattern, an `=`, and an expression
pub fn def<'a>(min_indent: u16) -> impl Parser<'a, Def<'a>, SyntaxError<'a>> {
    move |arena, state: State<'a>| {
        // specialize(|e, _, _| SyntaxError::Expr(e), def_help(min_indent))

        match def_help_help(min_indent).parse(arena, state) {
            Err((progress, fail, state)) => Err((progress, SyntaxError::Expr(fail), state)),
            Ok((progress, mut loc_defs, state)) => match loc_defs.pop() {
                Some(loc_def) => Ok((progress, loc_def.value, state)),
                None => panic!(),
            },
        }
    }
}

pub fn def_help_help<'a>(min_indent: u16) -> impl Parser<'a, Vec<'a, Located<Def<'a>>>, EExpr<'a>> {
    move |arena, state: State<'a>| {
        let def_state = DefState {
            defs: Vec::new_in(arena),
            spaces_after: &[],
        };

        let (_, initial_space, state) =
            space0_e(min_indent, EExpr::Space, EExpr::IndentEnd).parse(arena, state)?;

        let start = state.get_position();
        let (_, def_state, state) = parse_defs_end(start, def_state, arena, state)?;

        let (_, final_space, state) =
            space0_e(start.col, EExpr::Space, EExpr::IndentEnd).parse(arena, state)?;

        let mut output = Vec::with_capacity_in(def_state.defs.len(), arena);

        if !def_state.defs.is_empty() {
            let first = 0;
            let last = def_state.defs.len() - 1;

            for (i, ref_def) in def_state.defs.into_iter().enumerate() {
                let mut def = ref_def.clone();

                if i == first {
                    def = arena
                        .alloc(def.value)
                        .with_spaces_before(initial_space, def.region)
                }

                if i == last {
                    def = arena
                        .alloc(def.value)
                        .with_spaces_after(final_space, def.region)
                }

                output.push(def);
            }
        }

        Ok((MadeProgress, output, state))
    }
}

fn def_help<'a>(min_indent: u16) -> impl Parser<'a, Def<'a>, EExpr<'a>> {
    let indented_more = min_indent + 1;

    enum DefKind {
        Colon,
        Equal,
    }

    let def_colon_or_equals = one_of![
        map!(equals_with_indent_help(), |_| DefKind::Equal),
        map!(colon_with_indent(), |_| DefKind::Colon),
    ];

    then(
        // backtrackable because
        //
        // i = 0
        // i
        //
        // on the last line, we parse a pattern `i`, but it's not actually a def, so need to
        // backtrack
        and!(backtrackable(pattern_help(min_indent)), def_colon_or_equals),
        move |arena, state, _progress, (loc_pattern, def_kind)| match def_kind {
            DefKind::Colon => {
                // Spaces after the ':' (at a normal indentation level) and then the type.
                // The type itself must be indented more than the pattern and ':'
                let (_, ann_type, state) = specialize(
                    EExpr::Type,
                    space0_before_e(
                        type_annotation::located_help(indented_more),
                        min_indent,
                        Type::TSpace,
                        Type::TIndentStart,
                    ),
                )
                .parse(arena, state)?;

                // see if there is a definition (assuming the preceding characters were a type
                // annotation
                // TODO parse all the spaces, and check if we moved more than >= 1 line down
                let (_, opt_rest, state) = optional(and!(
                    spaces_till_end_of_line(),
                    body_at_indent_help(min_indent)
                ))
                .parse(arena, state)?;

                let def = match opt_rest {
                    None => {
                        annotation_or_alias(arena, &loc_pattern.value, loc_pattern.region, ann_type)
                    }
                    Some((opt_comment, (body_pattern, body_expr))) => Def::AnnotatedBody {
                        ann_pattern: arena.alloc(loc_pattern),
                        ann_type: arena.alloc(ann_type),
                        comment: opt_comment,
                        body_pattern: arena.alloc(body_pattern),
                        body_expr: arena.alloc(body_expr),
                    },
                };

                Ok((MadeProgress, def, state))
            }
            DefKind::Equal => {
                // Spaces after the '=' (at a normal indentation level) and then the expr.
                // The expr itself must be indented more than the pattern and '='
                let (_, body_expr, state) = space0_before_e(
                    move |arena, state| parse_expr_help(indented_more, arena, state),
                    min_indent,
                    EExpr::Space,
                    EExpr::IndentStart,
                )
                .parse(arena, state)?;

                Ok((
                    MadeProgress,
                    Def::Body(arena.alloc(loc_pattern), arena.alloc(body_expr)),
                    state,
                ))
            }
        },
    )
}

// PARSER HELPERS

fn pattern_help<'a>(min_indent: u16) -> impl Parser<'a, Located<Pattern<'a>>, EExpr<'a>> {
    specialize_ref(
        EExpr::Pattern,
        space0_after_e(
            loc_closure_param(min_indent),
            min_indent,
            EPattern::Space,
            EPattern::IndentStart,
        ),
    )
}

fn spaces_till_end_of_line<'a>() -> impl Parser<'a, Option<&'a str>, EExpr<'a>> {
    crate::blankspace::spaces_till_end_of_line(|r, c| {
        EExpr::Space(parser::BadInputError::HasTab, r, c)
    })
}

type Body<'a> = (Located<Pattern<'a>>, Located<Expr<'a>>);

fn body_at_indent_help<'a>(indent_level: u16) -> impl Parser<'a, Body<'a>, EExpr<'a>> {
    let indented_more = indent_level + 1;
    and!(
        skip_first!(spaces_exactly_e(indent_level), pattern_help(indent_level)),
        skip_first!(
            equals_with_indent_help(),
            // Spaces after the '=' (at a normal indentation level) and then the expr.
            // The expr itself must be indented more than the pattern and '='
            space0_before_e(
                move |arena, state| parse_expr_help(indented_more, arena, state),
                indent_level,
                EExpr::Space,
                EExpr::IndentStart,
            )
        )
    )
}

fn annotation_or_alias<'a>(
    arena: &'a Bump,
    pattern: &Pattern<'a>,
    pattern_region: Region,
    loc_ann: Located<TypeAnnotation<'a>>,
) -> Def<'a> {
    use crate::ast::Pattern::*;

    match pattern {
        // Type aliases initially parse as either global tags
        // or applied global tags, because they are always uppercase
        GlobalTag(name) => Def::Alias {
            name: Located {
                value: name,
                region: pattern_region,
            },
            vars: &[],
            ann: loc_ann,
        },
        Apply(
            Located {
                region: pattern_region,
                value: Pattern::GlobalTag(name),
            },
            loc_vars,
        ) => Def::Alias {
            name: Located {
                value: name,
                region: *pattern_region,
            },
            vars: loc_vars,
            ann: loc_ann,
        },
        Apply(_, _) => {
            Def::NotYetImplemented("TODO gracefully handle invalid Apply in type annotation")
        }
        SpaceAfter(value, spaces_before) => Def::SpaceAfter(
            arena.alloc(annotation_or_alias(arena, value, pattern_region, loc_ann)),
            spaces_before,
        ),
        SpaceBefore(value, spaces_before) => Def::SpaceBefore(
            arena.alloc(annotation_or_alias(arena, value, pattern_region, loc_ann)),
            spaces_before,
        ),
        Nested(value) => annotation_or_alias(arena, value, pattern_region, loc_ann),

        PrivateTag(_) => {
            Def::NotYetImplemented("TODO gracefully handle trying to use a private tag as an annotation.")
        }
        QualifiedIdentifier { .. } => {
            Def::NotYetImplemented("TODO gracefully handle trying to annotate a qualified identifier, e.g. `Foo.bar : ...`")
        }
        NumLiteral(_) | NonBase10Literal { .. } | FloatLiteral(_) | StrLiteral(_) => {
            Def::NotYetImplemented("TODO gracefully handle trying to annotate a litera")
        }
        Underscore(_) => {
            Def::NotYetImplemented("TODO gracefully handle trying to give a type annotation to an undrscore")
        }
        Malformed(_) => {
            Def::NotYetImplemented("TODO translate a malformed pattern into a malformed annotation")
        }
        MalformedIdent(_, _) => {
            Def::NotYetImplemented("TODO translate a malformed pattern into a malformed annotation")
        }
        Identifier(ident) => {
            // This is a regular Annotation
            Def::Annotation(
                Located {
                    region: pattern_region,
                    value: Pattern::Identifier(ident),
                },
                loc_ann,
            )
        }
        RecordDestructure(loc_patterns) => {
            // This is a record destructure Annotation
            Def::Annotation(
                Located {
                    region: pattern_region,
                    value: Pattern::RecordDestructure(loc_patterns),
                },
                loc_ann,
            )
        }
        RequiredField(_, _) | OptionalField(_, _) => {
            unreachable!("This should only be possible inside a record destruture.");
        }
    }
}

fn check_def_indent(
    min_indent: u16,
    def_start_column: u16,
    special_token_indent: u16,
    state: State,
) -> Result<State, (Progress, EExpr, State)> {
    if def_start_column < min_indent || special_token_indent < def_start_column {
        Err((
            NoProgress,
            EExpr::IndentDefBody(state.line, state.column),
            state,
        ))
    } else {
        Ok(state)
    }
}

fn parse_def_expr_help<'a>(
    min_indent: u16,
    def_start_col: u16,
    equals_sign_indent: u16,
    arena: &'a Bump,
    state: State<'a>,
    loc_first_pattern: Located<Pattern<'a>>,
    spaces_after_equals: &'a [CommentOrNewline<'a>],
) -> ParseResult<'a, Expr<'a>, EExpr<'a>> {
    let state = check_def_indent(min_indent, def_start_col, equals_sign_indent, state)?;

    // Indented more beyond the original indent of the entire def-expr.
    let indented_more = def_start_col + 1;

    then(
        and!(
            // Parse the body of the first def. It doesn't need any spaces
            // around it parsed, because both the subsquent defs and the
            // final body will have space1_before on them.
            //
            // It should be indented more than the original, and it will
            // end when outdented again.
            move |arena, state| parse_expr_help(indented_more, arena, state),
            and!(
                // Optionally parse additional defs.
                parse_defs_help(def_start_col),
                // Parse the final expression that will be returned.
                // It should be indented the same amount as the original.
                space0_before_e(
                    move |arena, state: State<'a>| { parse_expr_help(def_start_col, arena, state) },
                    def_start_col,
                    EExpr::Space,
                    EExpr::IndentStart,
                )
            )
        ),
        move |arena, state, progress, (loc_first_body, (mut defs, loc_ret))| {
            let loc_first_body = if spaces_after_equals.is_empty() {
                loc_first_body
            } else {
                Located {
                    value: Expr::SpaceBefore(
                        arena.alloc(loc_first_body.value),
                        spaces_after_equals,
                    ),
                    region: loc_first_body.region,
                }
            };
            let def_region = Region::span_across(&loc_first_pattern.region, &loc_first_body.region);

            let first_def: Def<'a> =
                    // TODO is there some way to eliminate this .clone() here?
                    Def::Body(arena.alloc(loc_first_pattern.clone()), arena.alloc(loc_first_body));

            let loc_first_def = Located {
                value: first_def,
                region: def_region,
            };

            // for formatting reasons, we must insert the first def first!
            defs.insert(0, &*arena.alloc(loc_first_def));

            Ok((
                progress,
                Expr::Defs(defs.into_bump_slice(), arena.alloc(loc_ret)),
                state,
            ))
        },
    )
    .parse(arena, state)
}

fn closure_help<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, ELambda<'a>> {
    map_with_arena!(
        skip_first!(
            // All closures start with a '\' - e.g. (\x -> x + 1)
            word1(b'\\', ELambda::Start),
            // Once we see the '\', we're committed to parsing this as a closure.
            // It may turn out to be malformed, but it is definitely a closure.
            and!(
                // Parse the params
                // Params are comma-separated
                sep_by1_e(
                    word1(b',', ELambda::Comma),
                    space0_around_ee(
                        specialize(ELambda::Pattern, loc_closure_param(min_indent)),
                        min_indent,
                        ELambda::Space,
                        ELambda::IndentArg,
                        ELambda::IndentArrow
                    ),
                    ELambda::Arg,
                ),
                skip_first!(
                    // Parse the -> which separates params from body
                    word2(b'-', b'>', ELambda::Arrow),
                    // Parse the body
                    space0_before_e(
                        specialize_ref(ELambda::Body, move |arena, state| parse_expr_help(
                            min_indent, arena, state
                        )),
                        min_indent,
                        ELambda::Space,
                        ELambda::IndentBody
                    )
                )
            )
        ),
        |arena: &'a Bump, (params, loc_body)| {
            let params: Vec<'a, Located<Pattern<'a>>> = params;
            let params: &'a [Located<Pattern<'a>>] = params.into_bump_slice();

            Expr::Closure(params, arena.alloc(loc_body))
        }
    )
}

mod when {
    use super::*;
    use crate::ast::WhenBranch;

    /// Parser for when expressions.
    pub fn expr_help<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, When<'a>> {
        then(
            and!(
                when_with_indent(),
                skip_second!(
                    space0_around_ee(
                        specialize_ref(When::Condition, move |arena, state| {
                            parse_expr_help(min_indent, arena, state)
                        }),
                        min_indent,
                        When::Space,
                        When::IndentCondition,
                        When::IndentIs,
                    ),
                    parser::keyword_e(keyword::IS, When::Is)
                )
            ),
            move |arena, state, progress, (case_indent, loc_condition)| {
                if case_indent < min_indent {
                    return Err((
                        progress,
                        // TODO maybe pass case_indent here?
                        When::PatternAlignment(5, state.line, state.column),
                        state,
                    ));
                }

                // Everything in the branches must be indented at least as much as the case itself.
                let min_indent = case_indent;

                let (p1, branches, state) = branches(min_indent).parse(arena, state)?;

                Ok((
                    progress.or(p1),
                    Expr::When(arena.alloc(loc_condition), branches.into_bump_slice()),
                    state,
                ))
            },
        )
    }

    /// Parsing when with indentation.
    fn when_with_indent<'a>() -> impl Parser<'a, u16, When<'a>> {
        move |arena, state: State<'a>| {
            parser::keyword_e(keyword::WHEN, When::When)
                .parse(arena, state)
                .map(|(progress, (), state)| (progress, state.indent_col, state))
        }
    }

    fn branches<'a>(min_indent: u16) -> impl Parser<'a, Vec<'a, &'a WhenBranch<'a>>, When<'a>> {
        move |arena, state| {
            let mut branches: Vec<'a, &'a WhenBranch<'a>> = Vec::with_capacity_in(2, arena);

            // 1. Parse the first branch and get its indentation level. (It must be >= min_indent.)
            // 2. Parse the other branches. Their indentation levels must be == the first branch's.

            let (_, (loc_first_patterns, loc_first_guard), state) =
                branch_alternatives(min_indent).parse(arena, state)?;
            let loc_first_pattern = loc_first_patterns.first().unwrap();
            let original_indent = loc_first_pattern.region.start_col;
            let indented_more = original_indent + 1;

            // Parse the first "->" and the expression after it.
            let (_, loc_first_expr, mut state) =
                branch_result(indented_more).parse(arena, state)?;

            // Record this as the first branch, then optionally parse additional branches.
            branches.push(arena.alloc(WhenBranch {
                patterns: loc_first_patterns.into_bump_slice(),
                value: loc_first_expr,
                guard: loc_first_guard,
            }));

            let branch_parser = map!(
                and!(
                    then(
                        branch_alternatives(min_indent),
                        move |_arena, state, _, (loc_patterns, loc_guard)| {
                            match alternatives_indented_correctly(&loc_patterns, original_indent) {
                                Ok(()) => Ok((MadeProgress, (loc_patterns, loc_guard), state)),
                                Err(indent) => Err((
                                    MadeProgress,
                                    When::PatternAlignment(indent, state.line, state.column),
                                    state,
                                )),
                            }
                        },
                    ),
                    branch_result(indented_more)
                ),
                |((patterns, guard), expr)| {
                    let patterns: Vec<'a, _> = patterns;
                    WhenBranch {
                        patterns: patterns.into_bump_slice(),
                        value: expr,
                        guard,
                    }
                }
            );

            while !state.bytes.is_empty() {
                match branch_parser.parse(arena, state) {
                    Ok((_, next_output, next_state)) => {
                        state = next_state;

                        branches.push(arena.alloc(next_output));
                    }
                    Err((MadeProgress, problem, old_state)) => {
                        return Err((MadeProgress, problem, old_state));
                    }
                    Err((NoProgress, _, old_state)) => {
                        state = old_state;

                        break;
                    }
                }
            }

            Ok((MadeProgress, branches, state))
        }
    }

    /// Parsing alternative patterns in when branches.
    fn branch_alternatives<'a>(
        min_indent: u16,
    ) -> impl Parser<'a, (Vec<'a, Located<Pattern<'a>>>, Option<Located<Expr<'a>>>), When<'a>> {
        and!(
            sep_by1(word1(b'|', When::Bar), |arena, state| {
                let (_, spaces, state) =
                    backtrackable(space0_e(min_indent, When::Space, When::IndentPattern))
                        .parse(arena, state)?;

                let (_, loc_pattern, state) = space0_after_e(
                    specialize(When::Pattern, crate::pattern::loc_pattern_help(min_indent)),
                    min_indent,
                    When::Space,
                    When::IndentPattern,
                )
                .parse(arena, state)?;

                Ok((
                    MadeProgress,
                    if spaces.is_empty() {
                        loc_pattern
                    } else {
                        arena
                            .alloc(loc_pattern.value)
                            .with_spaces_before(spaces, loc_pattern.region)
                    },
                    state,
                ))
            }),
            one_of![
                map!(
                    skip_first!(
                        parser::keyword_e(keyword::IF, When::IfToken),
                        // TODO we should require space before the expression but not after
                        space0_around_ee(
                            specialize_ref(When::IfGuard, move |arena, state| {
                                parse_expr_help(min_indent, arena, state)
                            }),
                            min_indent,
                            When::Space,
                            When::IndentIfGuard,
                            When::IndentArrow,
                        )
                    ),
                    Some
                ),
                |_, s| Ok((NoProgress, None, s))
            ]
        )
    }

    /// Check if alternatives of a when branch are indented correctly.
    fn alternatives_indented_correctly<'a>(
        loc_patterns: &'a Vec<'a, Located<Pattern<'a>>>,
        original_indent: u16,
    ) -> Result<(), u16> {
        let (first, rest) = loc_patterns.split_first().unwrap();
        let first_indented_correctly = first.region.start_col == original_indent;
        if first_indented_correctly {
            for when_pattern in rest.iter() {
                if when_pattern.region.start_col < original_indent {
                    return Err(original_indent - when_pattern.region.start_col);
                }
            }
            Ok(())
        } else {
            Err(original_indent - first.region.start_col)
        }
    }

    /// Parsing the righthandside of a branch in a when conditional.
    fn branch_result<'a>(indent: u16) -> impl Parser<'a, Located<Expr<'a>>, When<'a>> {
        skip_first!(
            word2(b'-', b'>', When::Arrow),
            space0_before_e(
                specialize_ref(When::Branch, move |arena, state| parse_expr_help(
                    indent, arena, state
                )),
                indent,
                When::Space,
                When::IndentBranch,
            )
        )
    }
}

fn if_branch<'a>(
    min_indent: u16,
) -> impl Parser<'a, (Located<Expr<'a>>, Located<Expr<'a>>), If<'a>> {
    move |arena, state| {
        // NOTE: only parse spaces before the expression
        let (_, cond, state) = space0_around_ee(
            specialize_ref(If::Condition, move |arena, state| {
                parse_expr_help(min_indent, arena, state)
            }),
            min_indent,
            If::Space,
            If::IndentCondition,
            If::IndentThenToken,
        )
        .parse(arena, state)
        .map_err(|(_, f, s)| (MadeProgress, f, s))?;

        let (_, _, state) = parser::keyword_e(keyword::THEN, If::Then)
            .parse(arena, state)
            .map_err(|(_, f, s)| (MadeProgress, f, s))?;

        let (_, then_branch, state) = space0_around_ee(
            specialize_ref(If::ThenBranch, move |arena, state| {
                parse_expr_help(min_indent, arena, state)
            }),
            min_indent,
            If::Space,
            If::IndentThenBranch,
            If::IndentElseToken,
        )
        .parse(arena, state)
        .map_err(|(_, f, s)| (MadeProgress, f, s))?;

        let (_, _, state) = parser::keyword_e(keyword::ELSE, If::Else)
            .parse(arena, state)
            .map_err(|(_, f, s)| (MadeProgress, f, s))?;

        Ok((MadeProgress, (cond, then_branch), state))
    }
}

fn if_expr_help<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, If<'a>> {
    move |arena: &'a Bump, state| {
        let (_, _, state) = parser::keyword_e(keyword::IF, If::If).parse(arena, state)?;

        let mut branches = Vec::with_capacity_in(1, arena);

        let mut loop_state = state;

        let state_final_else = loop {
            let (_, (cond, then_branch), state) = if_branch(min_indent).parse(arena, loop_state)?;

            branches.push((cond, then_branch));

            // try to parse another `if`
            // NOTE this drops spaces between the `else` and the `if`
            let optional_if = and!(
                backtrackable(space0_e(min_indent, If::Space, If::IndentIf)),
                parser::keyword_e(keyword::IF, If::If)
            );

            match optional_if.parse(arena, state) {
                Err((_, _, state)) => break state,
                Ok((_, _, state)) => {
                    loop_state = state;
                    continue;
                }
            }
        };

        let (_, else_branch, state) = space0_before_e(
            specialize_ref(If::ElseBranch, move |arena, state| {
                parse_expr_help(min_indent, arena, state)
            }),
            min_indent,
            If::Space,
            If::IndentElseBranch,
        )
        .parse(arena, state_final_else)
        .map_err(|(_, f, s)| (MadeProgress, f, s))?;

        let expr = Expr::If(branches.into_bump_slice(), arena.alloc(else_branch));

        Ok((MadeProgress, expr, state))
    }
}

/// This is a helper function for parsing function args.
/// The rules for (-) are special-cased, and they come up in function args.
///
/// They work like this:
///
/// x - y  # "x minus y"
/// x-y    # "x minus y"
/// x- y   # "x minus y" (probably written in a rush)
/// x -y   # "call x, passing (-y)"
///
/// Since operators have higher precedence than function application,
/// any time we encounter a '-' it is unary iff it is both preceded by spaces
/// and is *not* followed by a whitespace character.

/// When we parse an ident like `foo ` it could be any of these:
///
/// 1. A standalone variable with trailing whitespace (e.g. because an operator is next)
/// 2. The beginning of a function call (e.g. `foo bar baz`)
/// 3. The beginning of a definition (e.g. `foo =`)
/// 4. The beginning of a type annotation (e.g. `foo :`)
/// 5. A reserved keyword (e.g. `if ` or `case `), meaning we should do something else.

fn assign_or_destructure_identifier<'a>() -> impl Parser<'a, Ident<'a>, EExpr<'a>> {
    crate::ident::parse_ident_help
}

#[allow(dead_code)]
fn with_indent<'a, E, T, P>(parser: P) -> impl Parser<'a, u16, E>
where
    P: Parser<'a, T, E>,
    E: 'a,
{
    move |arena, state: State<'a>| {
        let indent_col = state.indent_col;

        let (progress, _, state) = parser.parse(arena, state)?;

        Ok((progress, indent_col, state))
    }
}

fn equals_with_indent_help<'a>() -> impl Parser<'a, u16, EExpr<'a>> {
    move |_arena, state: State<'a>| {
        let indent_col = state.indent_col;
        let good = state.bytes.starts_with(b"=") && !state.bytes.starts_with(b"==");

        if good {
            match state.advance_without_indenting_e(1, EExpr::Space) {
                Err(bad) => Err(bad),
                Ok(good) => Ok((MadeProgress, indent_col, good)),
            }
        } else {
            let equals = EExpr::Equals(state.line, state.column);
            Err((NoProgress, equals, state))
        }
    }
}

fn colon_with_indent<'a>() -> impl Parser<'a, u16, EExpr<'a>> {
    move |_arena, state: State<'a>| {
        let indent_col = state.indent_col;

        if let Some(b':') = state.bytes.get(0) {
            if let Some(b':') = state.bytes.get(1) {
                let double = EExpr::DoubleColon(state.line, state.column);
                Err((NoProgress, double, state))
            } else {
                match state.advance_without_indenting_e(1, EExpr::Space) {
                    Err(bad) => Err(bad),
                    Ok(good) => Ok((MadeProgress, indent_col, good)),
                }
            }
        } else {
            let colon = EExpr::Colon(state.line, state.column);
            Err((NoProgress, colon, state))
        }
    }
}

fn ident_to_expr<'a>(arena: &'a Bump, src: Ident<'a>) -> Expr<'a> {
    match src {
        Ident::GlobalTag(string) => Expr::GlobalTag(string),
        Ident::PrivateTag(string) => Expr::PrivateTag(string),
        Ident::Access { module_name, parts } => {
            let mut iter = parts.iter();

            // The first value in the iterator is the variable name,
            // e.g. `foo` in `foo.bar.baz`
            let mut answer = match iter.next() {
                Some(ident) => Expr::Var { module_name, ident },
                None => {
                    panic!("Parsed an Ident::Access with no parts");
                }
            };

            // The remaining items in the iterator are record field accesses,
            // e.g. `bar` in `foo.bar.baz`, followed by `baz`
            for field in iter {
                // Wrap the previous answer in the new one, so we end up
                // with a nested Expr. That way, `foo.bar.baz` gets represented
                // in the AST as if it had been written (foo.bar).baz all along.
                answer = Expr::Access(arena.alloc(answer), field);
            }

            answer
        }
        Ident::AccessorFunction(string) => Expr::AccessorFunction(string),
        Ident::Malformed(string, problem) => Expr::MalformedIdent(string, problem),
    }
}

fn list_literal_help<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, List<'a>> {
    move |arena, state| {
        let (_, (parsed_elems, final_comments), state) = collection_trailing_sep_e!(
            word1(b'[', List::Open),
            specialize_ref(List::Expr, loc!(expr_help(min_indent))),
            word1(b',', List::End),
            word1(b']', List::End),
            min_indent,
            List::Open,
            List::Space,
            List::IndentEnd
        )
        .parse(arena, state)?;

        let mut allocated = Vec::with_capacity_in(parsed_elems.len(), arena);

        for parsed_elem in parsed_elems {
            allocated.push(&*arena.alloc(parsed_elem));
        }

        let expr = Expr::List {
            items: allocated.into_bump_slice(),
            final_comments,
        };

        Ok((MadeProgress, expr, state))
    }
}

fn record_field_help<'a>(
    min_indent: u16,
) -> impl Parser<'a, AssignedField<'a, Expr<'a>>, ERecord<'a>> {
    use AssignedField::*;

    move |arena, state: State<'a>| {
        // You must have a field name, e.g. "email"
        let (progress, loc_label, state) =
            specialize(|_, r, c| ERecord::Field(r, c), loc!(lowercase_ident()))
                .parse(arena, state)?;
        debug_assert_eq!(progress, MadeProgress);

        let (_, spaces, state) =
            space0_e(min_indent, ERecord::Space, ERecord::IndentColon).parse(arena, state)?;

        // Having a value is optional; both `{ email }` and `{ email: blah }` work.
        // (This is true in both literals and types.)
        let (_, opt_loc_val, state) = optional(and!(
            either!(
                word1(b':', ERecord::Colon),
                word1(b'?', ERecord::QuestionMark)
            ),
            space0_before_e(
                specialize_ref(ERecord::Expr, loc!(expr_help(min_indent))),
                min_indent,
                ERecord::Space,
                ERecord::IndentEnd,
            )
        ))
        .parse(arena, state)?;

        let answer = match opt_loc_val {
            Some((Either::First(_), loc_val)) => {
                RequiredValue(loc_label, spaces, arena.alloc(loc_val))
            }

            Some((Either::Second(_), loc_val)) => {
                OptionalValue(loc_label, spaces, arena.alloc(loc_val))
            }

            // If no value was provided, record it as a Var.
            // Canonicalize will know what to do with a Var later.
            None => {
                if !spaces.is_empty() {
                    SpaceAfter(arena.alloc(LabelOnly(loc_label)), spaces)
                } else {
                    LabelOnly(loc_label)
                }
            }
        };

        Ok((MadeProgress, answer, state))
    }
}

fn record_updateable_identifier<'a>() -> impl Parser<'a, Expr<'a>, ERecord<'a>> {
    specialize(
        |_, r, c| ERecord::Updateable(r, c),
        map_with_arena!(parse_ident_help, ident_to_expr),
    )
}

fn record_help<'a>(
    min_indent: u16,
) -> impl Parser<
    'a,
    (
        Option<Located<Expr<'a>>>,
        Located<(
            Vec<'a, Located<AssignedField<'a, Expr<'a>>>>,
            &'a [CommentOrNewline<'a>],
        )>,
    ),
    ERecord<'a>,
> {
    skip_first!(
        word1(b'{', ERecord::Open),
        and!(
            // You can optionally have an identifier followed by an '&' to
            // make this a record update, e.g. { Foo.user & username: "blah" }.
            optional(skip_second!(
                space0_around_ee(
                    // We wrap the ident in an Expr here,
                    // so that we have a Spaceable value to work with,
                    // and then in canonicalization verify that it's an Expr::Var
                    // (and not e.g. an `Expr::Access`) and extract its string.
                    loc!(record_updateable_identifier()),
                    min_indent,
                    ERecord::Space,
                    ERecord::IndentEnd,
                    ERecord::IndentAmpersand,
                ),
                word1(b'&', ERecord::Ampersand)
            )),
            loc!(skip_first!(
                // We specifically allow space characters inside here, so that
                // `{  }` can be successfully parsed as an empty record, and then
                // changed by the formatter back into `{}`.
                zero_or_more!(word1(b' ', ERecord::End)),
                skip_second!(
                    and!(
                        trailing_sep_by0(
                            word1(b',', ERecord::End),
                            space0_around_ee(
                                loc!(record_field_help(min_indent)),
                                min_indent,
                                ERecord::Space,
                                ERecord::IndentEnd,
                                ERecord::IndentEnd
                            ),
                        ),
                        space0_e(min_indent, ERecord::Space, ERecord::IndentEnd)
                    ),
                    word1(b'}', ERecord::End)
                )
            ))
        )
    )
}

fn record_literal_help<'a>(min_indent: u16) -> impl Parser<'a, Expr<'a>, EExpr<'a>> {
    then(
        loc!(specialize(EExpr::Record, record_help(min_indent))),
        move |arena, state, _, loc_record| {
            let (opt_update, loc_assigned_fields_with_comments) = loc_record.value;

            // This is a record literal, not a destructure.
            let mut value = Expr::Record {
                update: opt_update.map(|loc_expr| &*arena.alloc(loc_expr)),
                fields: loc_assigned_fields_with_comments.value.0.into_bump_slice(),
                final_comments: loc_assigned_fields_with_comments.value.1,
            };

            // there can be field access, e.g. `{ x : 4 }.x`
            let (_, accesses, state) = optional(record_field_access_chain()).parse(arena, state)?;

            if let Some(fields) = accesses {
                for field in fields {
                    // Wrap the previous answer in the new one, so we end up
                    // with a nested Expr. That way, `foo.bar.baz` gets represented
                    // in the AST as if it had been written (foo.bar).baz all along.
                    value = Expr::Access(arena.alloc(value), field);
                }
            }

            Ok((MadeProgress, value, state))
        },
    )
}

fn string_literal_help<'a>() -> impl Parser<'a, Expr<'a>, EString<'a>> {
    map!(crate::string_literal::parse(), Expr::Str)
}

fn positive_number_literal_help<'a>() -> impl Parser<'a, Expr<'a>, Number> {
    map!(
        crate::number_literal::positive_number_literal(),
        |literal| {
            use crate::number_literal::NumLiteral::*;

            match literal {
                Num(s) => Expr::Num(s),
                Float(s) => Expr::Float(s),
                NonBase10Int {
                    string,
                    base,
                    is_negative,
                } => Expr::NonBase10Int {
                    string,
                    base,
                    is_negative,
                },
            }
        }
    )
}

fn number_literal_help<'a>() -> impl Parser<'a, Expr<'a>, Number> {
    map!(crate::number_literal::number_literal(), |literal| {
        use crate::number_literal::NumLiteral::*;

        match literal {
            Num(s) => Expr::Num(s),
            Float(s) => Expr::Float(s),
            NonBase10Int {
                string,
                base,
                is_negative,
            } => Expr::NonBase10Int {
                string,
                base,
                is_negative,
            },
        }
    })
}

const BINOP_CHAR_SET: &[u8] = b"+-/*=.<>:&|^?%!";

use crate::parser::{Col, Row};

fn operator<'a>() -> impl Parser<'a, BinOp, EExpr<'a>> {
    |_, state| operator_help(EExpr::Start, EExpr::BadOperator, state)
}

#[inline(always)]
fn operator_help<'a, F, G, E>(
    to_expectation: F,
    to_error: G,
    mut state: State<'a>,
) -> ParseResult<'a, BinOp, E>
where
    F: Fn(Row, Col) -> E,
    G: Fn(&'a [u8], Row, Col) -> E,
    E: 'a,
{
    let chomped = chomp_ops(state.bytes);

    macro_rules! good {
        ($op:expr, $width:expr) => {{
            state.column += $width;
            state.bytes = &state.bytes[$width..];

            Ok((MadeProgress, $op, state))
        }};
    }

    macro_rules! bad_made_progress {
        ($op:expr) => {{
            Err((MadeProgress, to_error($op, state.line, state.column), state))
        }};
    }

    match chomped {
        0 => Err((NoProgress, to_expectation(state.line, state.column), state)),
        1 => {
            let op = state.bytes[0];
            match op {
                b'+' => good!(BinOp::Plus, 1),
                b'-' => good!(BinOp::Minus, 1),
                b'*' => good!(BinOp::Star, 1),
                b'/' => good!(BinOp::Slash, 1),
                b'%' => good!(BinOp::Percent, 1),
                b'^' => good!(BinOp::Caret, 1),
                b'>' => good!(BinOp::GreaterThan, 1),
                b'<' => good!(BinOp::LessThan, 1),
                b'.' => {
                    // a `.` makes no progress, so it does not interfere with `.foo` access(or)
                    Err((NoProgress, to_error(b".", state.line, state.column), state))
                }
                b'=' => good!(BinOp::Assignment, 1),
                b':' => good!(BinOp::HasType, 1),
                _ => bad_made_progress!(&state.bytes[0..1]),
            }
        }
        2 => {
            let op0 = state.bytes[0];
            let op1 = state.bytes[1];

            match (op0, op1) {
                (b'|', b'>') => good!(BinOp::Pizza, 2),
                (b'=', b'=') => good!(BinOp::Equals, 2),
                (b'!', b'=') => good!(BinOp::NotEquals, 2),
                (b'>', b'=') => good!(BinOp::GreaterThanOrEq, 2),
                (b'<', b'=') => good!(BinOp::LessThanOrEq, 2),
                (b'&', b'&') => good!(BinOp::And, 2),
                (b'|', b'|') => good!(BinOp::Or, 2),
                (b'/', b'/') => good!(BinOp::DoubleSlash, 2),
                (b'%', b'%') => good!(BinOp::DoublePercent, 2),
                (b'-', b'>') => {
                    // makes no progress, so it does not interfere with `_ if isGood -> ...`
                    Err((NoProgress, to_error(b"->", state.line, state.column), state))
                }
                (b'<', b'-') => good!(BinOp::Backpassing, 2),
                _ => bad_made_progress!(&state.bytes[0..2]),
            }
        }
        _ => bad_made_progress!(&state.bytes[0..chomped]),
    }
}

fn chomp_ops(bytes: &[u8]) -> usize {
    let mut chomped = 0;

    for c in bytes.iter() {
        if !BINOP_CHAR_SET.contains(c) {
            return chomped;
        }
        chomped += 1;
    }

    chomped
}
