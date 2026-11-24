use super::ParseResult;
use super::Parser;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::Case;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::Expr;
use crate::gql::ast::FunctionCall;
use crate::gql::ast::ListComprehension;
use crate::gql::ast::ListPredicate;
use crate::gql::ast::Literal;
use crate::gql::ast::Name;
use crate::gql::ast::PatternComprehension;
use crate::gql::ast::Quantifier;
use crate::gql::ast::Reduce;
use crate::gql::ast::Subquery;
use crate::gql::ast::UnaryOp;
use crate::gql::ast::WhenOperand;
use crate::gql::ast::WhenThen;
use crate::gql::error::SyntaxError;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::lexer::Punct;
use crate::gql::lexer::TokenKind;

const RESERVED_WORDS: [&str; 26] = [
    "MATCH", "OPTIONAL", "WHERE", "RETURN", "WITH", "UNWIND", "CREATE", "INSERT", "MERGE",
    "DELETE", "DETACH", "SET", "REMOVE", "UNION", "ORDER", "SKIP", "LIMIT", "OFFSET", "WHEN",
    "THEN", "ELSE", "END", "AS", "AND", "OR", "XOR",
];

const NOT_PRECEDENCE: u8 = 4;
const COMPARISON_PRECEDENCE: u8 = 5;
const PREDICATE_PRECEDENCE: u8 = 6;

#[derive(Clone, Copy)]
enum Infix {
    Binary(BinaryOp, usize),
    Comparison(ComparisonOp),
    Is,
}

impl Infix {
    fn precedence(self) -> u8 {
        match self {
            Self::Binary(BinaryOp::Or, _) => 1,
            Self::Binary(BinaryOp::Xor, _) => 2,
            Self::Binary(BinaryOp::And, _) => 3,
            Self::Binary(
                BinaryOp::In
                | BinaryOp::StartsWith
                | BinaryOp::EndsWith
                | BinaryOp::Contains
                | BinaryOp::RegexMatch,
                _,
            )
            | Self::Is => PREDICATE_PRECEDENCE,
            Self::Comparison(_) => COMPARISON_PRECEDENCE,
            Self::Binary(BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Concat, _) => 7,
            Self::Binary(BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Modulo, _) => 8,
            Self::Binary(BinaryOp::Power, _) => 9,
        }
    }
}

impl Parser {
    pub(super) fn expression(&mut self) -> ParseResult<Expr> {
        self.binary(0)
    }

    pub(super) fn predicate(&mut self) -> ParseResult<Expr> {
        self.condition(false)
    }

    fn condition(&mut self, pipe_ends: bool) -> ParseResult<Expr> {
        let outer = std::mem::replace(&mut self.in_where, true);
        let ends = pipe_ends.then(|| self.bracket_depth());
        let condition = self.pipe_ending(ends, Self::expression);
        self.in_where = outer;
        condition
    }

    fn pipe_ending<T>(
        &mut self,
        ends: Option<usize>,
        parse: impl FnOnce(&mut Self) -> ParseResult<T>,
    ) -> ParseResult<T> {
        let outer = std::mem::replace(&mut self.pipe_ends_filter, ends);
        let parsed = parse(self);
        self.pipe_ends_filter = outer;
        parsed
    }

    fn adjacent_pipes(&self) -> bool {
        match (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)) {
            (Some(first), Some(second)) => {
                second.kind == TokenKind::Punct(Punct::Pipe) && first.end == second.start
            }
            _ => false,
        }
    }

    fn binary(&mut self, min_precedence: u8) -> ParseResult<Expr> {
        self.nested(|parser| parser.binary_chain(min_precedence))
    }

    fn binary_chain(&mut self, min_precedence: u8) -> ParseResult<Expr> {
        let mut lhs = self.prefix()?;
        let mut in_comparison_chain = false;
        while let Some(infix) = self.peek_infix() {
            let precedence = infix.precedence();
            if precedence < min_precedence {
                break;
            }
            self.deepen()?;
            lhs = match infix {
                Infix::Is => {
                    in_comparison_chain = false;
                    self.is_suffix(lhs)?
                }
                Infix::Comparison(op) => {
                    let chained = std::mem::replace(&mut in_comparison_chain, true);
                    self.comparison_rest(op, precedence, lhs, chained)?
                }
                Infix::Binary(op, token_count) => {
                    in_comparison_chain = false;
                    self.binary_rest(op, token_count, precedence, lhs)?
                }
            };
        }
        Ok(lhs)
    }

    fn is_suffix(&mut self, lhs: Expr) -> ParseResult<Expr> {
        self.pos += 1;
        let negated = self.eat_keyword("NOT");
        if self.eat_keyword("LABELED") {
            let test = Expr::HasLabels {
                operand: Box::new(lhs),
                labels: self.label_expression(self.label_pipe_allowed())?,
            };
            Ok(if negated {
                Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(test),
                }
            } else {
                test
            })
        } else {
            self.expect_keyword("NULL")?;
            Ok(Expr::IsNull {
                operand: Box::new(lhs),
                negated,
            })
        }
    }

    fn comparison_rest(
        &mut self,
        op: ComparisonOp,
        precedence: u8,
        lhs: Expr,
        chained: bool,
    ) -> ParseResult<Expr> {
        self.pos += 1;
        let rhs = self.binary(precedence + 1)?;
        Ok(match lhs {
            Expr::Comparison { first, mut rest } if chained => {
                rest.push((op, rhs));
                Expr::Comparison { first, rest }
            }
            first => Expr::Comparison {
                first: Box::new(first),
                rest: vec![(op, rhs)],
            },
        })
    }

    fn binary_rest(
        &mut self,
        op: BinaryOp,
        token_count: usize,
        precedence: u8,
        lhs: Expr,
    ) -> ParseResult<Expr> {
        self.pos += token_count;
        let rhs = self.binary(precedence + 1)?;
        Ok(Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    }

    fn peek_infix(&self) -> Option<Infix> {
        match self.kind_at(0)? {
            TokenKind::Punct(punct) => Some(match punct {
                Punct::Plus => Infix::Binary(BinaryOp::Add, 1),
                Punct::Minus => Infix::Binary(BinaryOp::Subtract, 1),
                Punct::Star => Infix::Binary(BinaryOp::Multiply, 1),
                Punct::Slash => Infix::Binary(BinaryOp::Divide, 1),
                Punct::Percent => Infix::Binary(BinaryOp::Modulo, 1),
                Punct::Caret => Infix::Binary(BinaryOp::Power, 1),
                Punct::RegexMatch => Infix::Binary(BinaryOp::RegexMatch, 1),
                Punct::Pipe if self.adjacent_pipes() => Infix::Binary(BinaryOp::Concat, 2),
                Punct::Eq => Infix::Comparison(ComparisonOp::Equal),
                Punct::Neq => Infix::Comparison(ComparisonOp::NotEqual),
                Punct::Lt => Infix::Comparison(ComparisonOp::Less),
                Punct::Le => Infix::Comparison(ComparisonOp::LessEqual),
                Punct::Gt => Infix::Comparison(ComparisonOp::Greater),
                Punct::Ge => Infix::Comparison(ComparisonOp::GreaterEqual),
                _ => return None,
            }),
            TokenKind::Word {
                quoted: false,
                text,
            } => {
                let word = |keyword: &str| text.eq_ignore_ascii_case(keyword);
                if word("OR") {
                    Some(Infix::Binary(BinaryOp::Or, 1))
                } else if word("XOR") {
                    Some(Infix::Binary(BinaryOp::Xor, 1))
                } else if word("AND") {
                    Some(Infix::Binary(BinaryOp::And, 1))
                } else if word("IN") {
                    Some(Infix::Binary(BinaryOp::In, 1))
                } else if word("CONTAINS") {
                    Some(Infix::Binary(BinaryOp::Contains, 1))
                } else if word("IS") {
                    Some(Infix::Is)
                } else if word("STARTS") && self.keyword_at(1, "WITH") {
                    Some(Infix::Binary(BinaryOp::StartsWith, 2))
                } else if word("ENDS") && self.keyword_at(1, "WITH") {
                    Some(Infix::Binary(BinaryOp::EndsWith, 2))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn prefix(&mut self) -> ParseResult<Expr> {
        if self.eat_keyword("NOT") {
            let operand = self.binary(NOT_PRECEDENCE + 1)?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(operand),
            });
        }
        self.nested(Self::signed)
    }

    fn signed(&mut self) -> ParseResult<Expr> {
        if self.at_punct(Punct::Minus) {
            if let Some(TokenKind::Integer(magnitude)) = self.kind_at(1)
                && *magnitude == i64::MIN.unsigned_abs()
            {
                self.pos += 2;
                return Ok(Expr::Literal(Literal::Integer(i64::MIN)));
            }
            self.pos += 1;
            let operand = self.nested(Self::signed)?;
            return Ok(negate(operand));
        }
        if self.eat_punct(Punct::Plus) {
            let operand = self.nested(Self::signed)?;
            return Ok(Expr::Unary {
                op: UnaryOp::Plus,
                operand: Box::new(operand),
            });
        }
        self.postfix_expression()
    }

    pub(super) fn postfix_expression(&mut self) -> ParseResult<Expr> {
        let before = self.depth;
        let mut expr = self.atom()?;
        loop {
            if self.eat_punct(Punct::Dot) {
                let key = self.name()?;
                expr = Expr::Property {
                    base: Box::new(expr),
                    key,
                };
            } else if self.eat_punct(Punct::LBracket) {
                expr = self.subscript(expr)?;
            } else if self.at_punct(Punct::Colon) && self.label_starts_at(1) {
                let labels = self.colon_labels(true, self.label_pipe_allowed())?;
                expr = Expr::HasLabels {
                    operand: Box::new(expr),
                    labels,
                };
            } else {
                break;
            }
            self.deepen()?;
        }
        self.depth = before;
        Ok(expr)
    }

    fn subscript(&mut self, base: Expr) -> ParseResult<Expr> {
        let base = Box::new(base);
        let from = if self.at_punct(Punct::DotDot) {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let expr = if self.eat_punct(Punct::DotDot) {
            let to = if self.at_punct(Punct::RBracket) {
                None
            } else {
                Some(Box::new(self.expression()?))
            };
            Expr::Slice { base, from, to }
        } else {
            let index = from.ok_or_else(|| self.unexpected())?;
            Expr::Index { base, index }
        };
        self.expect_punct(Punct::RBracket)?;
        Ok(expr)
    }

    fn atom(&mut self) -> ParseResult<Expr> {
        let Some(token) = self.peek() else {
            return Err(self.unexpected());
        };
        let start = token.start;
        match token.kind.clone() {
            TokenKind::Integer(magnitude) => {
                let value = i64::try_from(magnitude)
                    .map_err(|_| SyntaxError::new(start, SyntaxErrorKind::IntegerOverflow))?;
                self.pos += 1;
                Ok(Expr::Literal(Literal::Integer(value)))
            }
            TokenKind::Float(value) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::Float(value)))
            }
            TokenKind::InvalidNumber => Err(SyntaxError::new(
                start,
                SyntaxErrorKind::InvalidNumberLiteral,
            )),
            TokenKind::String(text) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::String(text)))
            }
            TokenKind::Parameter(name) => {
                self.pos += 1;
                Ok(Expr::Parameter(Name::new(name)))
            }
            TokenKind::Punct(Punct::LParen) => self.parenthesized(),
            TokenKind::Punct(Punct::LBracket) => {
                self.pos += 1;
                self.list()
            }
            TokenKind::Punct(Punct::LBrace) => Ok(Expr::Map(self.map_entries()?)),
            TokenKind::Word { text, quoted } => self.word(&text, quoted),
            TokenKind::Punct(_) => Err(self.unexpected()),
        }
    }

    fn parenthesized(&mut self) -> ParseResult<Expr> {
        if self.paren_starts_pattern() {
            if !self.in_where {
                return Err(self.unexpected());
            }
            return Ok(Expr::Pattern(Box::new(self.pattern_element()?)));
        }
        self.pos += 1;
        let inner = self.expression()?;
        self.expect_punct(Punct::RParen)?;
        Ok(inner)
    }

    fn paren_starts_pattern(&self) -> bool {
        let mut open = 0usize;
        for (index, token) in self.tokens.iter().enumerate().skip(self.pos) {
            match token.kind {
                TokenKind::Punct(Punct::LParen) => open += 1,
                TokenKind::Punct(Punct::RParen) => {
                    open = open.saturating_sub(1);
                    if open == 0 {
                        return self.relationship_starts_at(index + 1);
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn list(&mut self) -> ParseResult<Expr> {
        if self.eat_punct(Punct::RBracket) {
            return Ok(Expr::List(Vec::new()));
        }
        if self.at_pattern_comprehension() {
            return self.pattern_comprehension();
        }
        if self.at_word() && self.keyword_at(1, "IN") {
            let variable = self.name()?;
            self.pos += 1;
            let source = Box::new(self.expression()?);
            let filter = self.optional_predicate()?;
            let projection = if self.eat_punct(Punct::Pipe) {
                Some(Box::new(self.expression()?))
            } else {
                None
            };
            self.expect_punct(Punct::RBracket)?;
            return Ok(Expr::ListComprehension(ListComprehension {
                variable,
                source,
                filter,
                projection,
            }));
        }
        let items = self.comma_separated(Self::expression)?;
        self.expect_punct(Punct::RBracket)?;
        Ok(Expr::List(items))
    }

    fn at_pattern_comprehension(&self) -> bool {
        (self.at_punct(Punct::LParen) && self.paren_starts_pattern())
            || (self.at_word() && self.punct_at(1, Punct::Eq) && self.punct_at(2, Punct::LParen))
    }

    fn pattern_comprehension(&mut self) -> ParseResult<Expr> {
        let path_variable = if self.at_word() {
            let name = self.name()?;
            self.pos += 1;
            Some(name)
        } else {
            None
        };
        let element = self.pattern_element()?;
        let filter = self.optional_predicate()?;
        self.expect_punct(Punct::Pipe)?;
        let projection = Box::new(self.expression()?);
        self.expect_punct(Punct::RBracket)?;
        Ok(Expr::PatternComprehension(Box::new(PatternComprehension {
            path_variable,
            element,
            filter,
            projection,
        })))
    }

    fn collect_subquery(&mut self) -> ParseResult<Expr> {
        self.pos += 2;
        if !self.at_clause_start() {
            return Err(self.unexpected());
        }
        let query = self.nested(Self::single_query)?;
        self.expect_punct(Punct::RBrace)?;
        Ok(Expr::Collect(Box::new(query)))
    }

    fn subquery_body(&mut self) -> ParseResult<Subquery> {
        self.pos += 2;
        let body = if self.at_clause_start() {
            Subquery::Query(self.nested(Self::single_query)?)
        } else {
            let patterns = self.pattern_parts()?;
            let filter = if self.eat_keyword("WHERE") {
                Some(self.predicate()?)
            } else {
                None
            };
            Subquery::Pattern { patterns, filter }
        };
        self.expect_punct(Punct::RBrace)?;
        Ok(body)
    }

    fn optional_predicate(&mut self) -> ParseResult<Option<Box<Expr>>> {
        if self.eat_keyword("WHERE") {
            Ok(Some(Box::new(self.condition(true)?)))
        } else {
            Ok(None)
        }
    }

    fn optional_after_keyword(&mut self, keyword: &str) -> ParseResult<Option<Box<Expr>>> {
        if self.eat_keyword(keyword) {
            Ok(Some(Box::new(self.expression()?)))
        } else {
            Ok(None)
        }
    }

    fn word(&mut self, text: &str, quoted: bool) -> ParseResult<Expr> {
        if !quoted {
            let is = |keyword: &str| text.eq_ignore_ascii_case(keyword);
            if is("TRUE") {
                self.pos += 1;
                return Ok(Expr::Literal(Literal::Boolean(true)));
            }
            if is("FALSE") {
                self.pos += 1;
                return Ok(Expr::Literal(Literal::Boolean(false)));
            }
            if is("NULL") {
                self.pos += 1;
                return Ok(Expr::Literal(Literal::Null));
            }
            if is("CASE") {
                self.pos += 1;
                return self.pipe_ending(None, Self::case);
            }
            if self.punct_at(1, Punct::LBrace) {
                if is("EXISTS") {
                    return Ok(Expr::Exists(Box::new(self.subquery_body()?)));
                }
                if is("COUNT") {
                    return Ok(Expr::Count(Box::new(self.subquery_body()?)));
                }
                if is("COLLECT") {
                    return self.collect_subquery();
                }
            }
        }
        if self.at_function_call() {
            return self.function_call();
        }
        if !quoted
            && RESERVED_WORDS
                .iter()
                .any(|word| text.eq_ignore_ascii_case(word))
        {
            return Err(self.unexpected());
        }
        self.pos += 1;
        Ok(Expr::Variable(Name::new(text)))
    }

    fn at_function_call(&self) -> bool {
        let mut ahead = 0;
        loop {
            if !matches!(self.kind_at(ahead), Some(TokenKind::Word { .. })) {
                return false;
            }
            if self.punct_at(ahead + 1, Punct::LParen) {
                return true;
            }
            if !self.punct_at(ahead + 1, Punct::Dot) {
                return false;
            }
            ahead += 2;
        }
    }

    fn function_call(&mut self) -> ParseResult<Expr> {
        let mut namespace = vec![self.name()?];
        while self.eat_punct(Punct::Dot) {
            namespace.push(self.name()?);
        }
        let name = namespace.pop().ok_or_else(|| self.unexpected())?;
        self.expect_punct(Punct::LParen)?;
        if namespace.is_empty() {
            if let Some(quantifier) = quantifier_named(&name)
                && self.at_word()
                && self.keyword_at(1, "IN")
            {
                return self.list_predicate(quantifier);
            }
            if name.0.eq_ignore_ascii_case("reduce")
                && self.at_word()
                && self.punct_at(1, Punct::Eq)
            {
                return self.reduce();
            }
            if name.0.eq_ignore_ascii_case("trim")
                && let Some(call) = self.trim_from()?
            {
                return Ok(call);
            }
            if name.0.eq_ignore_ascii_case("count")
                && self.at_punct(Punct::Star)
                && self.punct_at(1, Punct::RParen)
            {
                self.pos += 2;
                return Ok(Expr::CountStar);
            }
        }
        let distinct = self.eat_keyword("DISTINCT");
        let args = if self.at_punct(Punct::RParen) {
            Vec::new()
        } else {
            self.comma_separated(Self::expression)?
        };
        self.expect_punct(Punct::RParen)?;
        Ok(Expr::Function(Box::new(FunctionCall {
            namespace,
            name,
            distinct,
            args,
        })))
    }

    fn trim_from(&mut self) -> ParseResult<Option<Expr>> {
        let start = self.pos;
        let side = ["LEADING", "TRAILING", "BOTH"]
            .into_iter()
            .find(|side| self.eat_keyword(side));
        if side.is_some() && self.at_punct(Punct::RParen) {
            self.pos = start;
            return Ok(None);
        }
        let characters = if self.at_keyword("FROM") {
            None
        } else {
            let characters = self.expression()?;
            if side.is_none() && !self.at_keyword("FROM") {
                self.pos = start;
                return Ok(None);
            }
            Some(characters)
        };
        self.expect_keyword("FROM")?;
        let source = self.expression()?;
        self.expect_punct(Punct::RParen)?;
        let name = match side {
            Some("LEADING") => "ltrim",
            Some("TRAILING") => "rtrim",
            _ => "btrim",
        };
        Ok(Some(Expr::Function(Box::new(FunctionCall {
            namespace: Vec::new(),
            name: Name::new(name),
            distinct: false,
            args: std::iter::once(source).chain(characters).collect(),
        }))))
    }

    fn reduce(&mut self) -> ParseResult<Expr> {
        let accumulator = self.name()?;
        self.pos += 1;
        let init = self.expression()?;
        self.expect_punct(Punct::Comma)?;
        if !self.at_word() || !self.keyword_at(1, "IN") {
            return Err(self.unexpected());
        }
        let variable = self.name()?;
        self.pos += 1;
        let source = self.expression()?;
        self.expect_punct(Punct::Pipe)?;
        let body = self.expression()?;
        self.expect_punct(Punct::RParen)?;
        Ok(Expr::Reduce(Box::new(Reduce {
            accumulator,
            init,
            variable,
            source,
            body,
        })))
    }

    fn list_predicate(&mut self, quantifier: Quantifier) -> ParseResult<Expr> {
        let variable = self.name()?;
        self.pos += 1;
        let source = Box::new(self.expression()?);
        self.expect_keyword("WHERE")?;
        let predicate = Box::new(self.predicate()?);
        self.expect_punct(Punct::RParen)?;
        Ok(Expr::ListPredicate(ListPredicate {
            quantifier,
            variable,
            source,
            predicate,
        }))
    }

    fn when_operand(&mut self) -> ParseResult<WhenOperand> {
        let op = match self.kind_at(0) {
            Some(TokenKind::Punct(Punct::Eq)) => Some(ComparisonOp::Equal),
            Some(TokenKind::Punct(Punct::Neq)) => Some(ComparisonOp::NotEqual),
            Some(TokenKind::Punct(Punct::Lt)) => Some(ComparisonOp::Less),
            Some(TokenKind::Punct(Punct::Le)) => Some(ComparisonOp::LessEqual),
            Some(TokenKind::Punct(Punct::Gt)) => Some(ComparisonOp::Greater),
            Some(TokenKind::Punct(Punct::Ge)) => Some(ComparisonOp::GreaterEqual),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            return Ok(WhenOperand::Compare(
                op,
                self.binary(COMPARISON_PRECEDENCE + 1)?,
            ));
        }
        if self.at_keyword("IS") && (self.keyword_at(1, "NULL") || self.keyword_at(1, "NOT")) {
            self.pos += 1;
            let negated = self.eat_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(WhenOperand::IsNull { negated });
        }
        Ok(WhenOperand::Value(self.expression()?))
    }

    fn case(&mut self) -> ParseResult<Expr> {
        let scrutinee = if self.at_keyword("WHEN") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let mut branches = Vec::new();
        while self.eat_keyword("WHEN") {
            let when = if scrutinee.is_some() {
                self.comma_separated(Self::when_operand)?
            } else {
                vec![WhenOperand::Value(self.expression()?)]
            };
            self.expect_keyword("THEN")?;
            let then = self.expression()?;
            branches.push(WhenThen { when, then });
        }
        if branches.is_empty() {
            return Err(self.unexpected());
        }
        let otherwise = self.optional_after_keyword("ELSE")?;
        self.expect_keyword("END")?;
        Ok(Expr::Case(Case {
            scrutinee,
            branches,
            otherwise,
        }))
    }
}

fn negate(operand: Expr) -> Expr {
    match operand {
        Expr::Literal(Literal::Integer(value)) if value.checked_neg().is_some() => {
            Expr::Literal(Literal::Integer(-value))
        }
        Expr::Literal(Literal::Float(value)) => Expr::Literal(Literal::Float(-value)),
        other => Expr::Unary {
            op: UnaryOp::Negate,
            operand: Box::new(other),
        },
    }
}

fn quantifier_named(name: &Name) -> Option<Quantifier> {
    let is = |keyword: &str| name.0.eq_ignore_ascii_case(keyword);
    if is("all") {
        Some(Quantifier::All)
    } else if is("any") {
        Some(Quantifier::Any)
    } else if is("none") {
        Some(Quantifier::None)
    } else if is("single") {
        Some(Quantifier::Single)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "expression.test.rs"]
mod tests;
