mod clause;
mod expression;
mod label;
mod pattern;

use super::ast::Call;
use super::ast::Clause;
use super::ast::Name;
use super::ast::Query;
use super::ast::SingleQuery;
use super::ast::Union;
use super::ast::UnionKind;
use super::ast::Yield;
use super::error::SyntaxError;
use super::error::SyntaxErrorKind;
use super::lexer::Punct;
use super::lexer::Token;
use super::lexer::TokenKind;
use super::lexer::lex;

const MAX_DEPTH: usize = 128;

pub(crate) type ParseResult<T> = Result<T, SyntaxError>;

/// Parses one query. Never panics: malformed input, including deeply nested
/// input, comes back as a `SyntaxError`.
pub(crate) fn parse(input: &str) -> ParseResult<Query> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        bracket_depths: bracket_depths(&tokens),
        tokens,
        pos: 0,
        depth: 0,
        input_len: input.len(),
        in_where: false,
        pipe_ends_filter: None,
    };
    parser.query()
}

fn bracket_depths(tokens: &[Token]) -> Vec<usize> {
    let mut depth = 0usize;
    tokens
        .iter()
        .map(|token| match token.kind {
            TokenKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => {
                depth += 1;
                depth - 1
            }
            TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                depth = depth.saturating_sub(1);
                depth
            }
            _ => depth,
        })
        .collect()
}

struct Parser {
    tokens: Vec<Token>,
    bracket_depths: Vec<usize>,
    pos: usize,
    depth: usize,
    input_len: usize,
    in_where: bool,
    pipe_ends_filter: Option<usize>,
}

impl Parser {
    fn query(&mut self) -> ParseResult<Query> {
        let query = self.union_query()?;
        self.eat_punct(Punct::Semicolon);
        if self.peek().is_some() {
            return Err(self.unexpected());
        }
        Ok(query)
    }

    fn union_query(&mut self) -> ParseResult<Query> {
        let first = self.single_query()?;
        let mut rest = Vec::new();
        let mut kind = None;
        while ["UNION", "EXCEPT", "INTERSECT", "OTHERWISE"]
            .iter()
            .any(|keyword| self.at_keyword(keyword))
        {
            let union_offset = self.offset();
            let operator = self.eat_keyword("UNION").then_some("UNION").or_else(|| {
                ["EXCEPT", "INTERSECT", "OTHERWISE"]
                    .into_iter()
                    .find(|keyword| self.eat_keyword(keyword))
            });
            let all = operator != Some("OTHERWISE") && self.eat_keyword("ALL");
            if !all && operator != Some("OTHERWISE") {
                self.eat_keyword("DISTINCT");
            }
            let this = match operator {
                Some("UNION") if all => UnionKind::All,
                Some("UNION") => UnionKind::Distinct,
                Some("EXCEPT") => UnionKind::Except { all },
                Some("INTERSECT") => UnionKind::Intersect { all },
                _ => UnionKind::Otherwise,
            };
            if kind.is_some_and(|earlier| earlier != this) {
                return Err(SyntaxError::new(union_offset, SyntaxErrorKind::MixedUnion));
            }
            kind = Some(this);
            rest.push(self.single_query()?);
        }
        Ok(match kind {
            Some(kind) => Query::Union(Union { kind, first, rest }),
            None => Query::Single(first),
        })
    }

    fn single_query(&mut self) -> ParseResult<SingleQuery> {
        let yields_star = |clause: &Clause| {
            matches!(
                clause,
                Clause::Call(Call {
                    yields: Some(Yield::Star),
                    ..
                })
            )
        };
        let mut clauses = Vec::new();
        let mut star = false;
        while clauses.is_empty() || self.at_clause_start() {
            if star {
                return Err(self.unexpected());
            }
            let clause = self.clause()?;
            star = yields_star(&clause);
            clauses.push(clause);
        }
        if clauses.len() > 1 && star {
            let star = self
                .pos
                .checked_sub(1)
                .and_then(|index| self.tokens.get(index))
                .map_or(0, |token| token.start);
            return Err(SyntaxError::new(star, SyntaxErrorKind::UnexpectedToken));
        }
        Ok(SingleQuery { clauses })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn kind_at(&self, ahead: usize) -> Option<&TokenKind> {
        self.tokens.get(self.pos + ahead).map(|token| &token.kind)
    }

    fn bracket_depth(&self) -> usize {
        self.bracket_depths.get(self.pos).copied().unwrap_or(0)
    }

    fn label_pipe_allowed(&self) -> bool {
        self.pipe_ends_filter != Some(self.bracket_depth())
    }

    fn offset(&self) -> usize {
        self.peek().map_or(self.input_len, |token| token.start)
    }

    fn previous_end(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .map_or(0, |token| token.end)
    }

    fn unexpected(&self) -> SyntaxError {
        let kind = if self.peek().is_some() {
            SyntaxErrorKind::UnexpectedToken
        } else {
            SyntaxErrorKind::UnexpectedEnd
        };
        SyntaxError::new(self.offset(), kind)
    }

    fn unsupported(&self) -> SyntaxError {
        SyntaxError::new(self.offset(), SyntaxErrorKind::UnsupportedFeature)
    }

    fn at_punct(&self, punct: Punct) -> bool {
        self.punct_at(0, punct)
    }

    fn punct_at(&self, ahead: usize, punct: Punct) -> bool {
        self.kind_at(ahead) == Some(&TokenKind::Punct(punct))
    }

    fn eat_punct(&mut self, punct: Punct) -> bool {
        let found = self.at_punct(punct);
        if found {
            self.pos += 1;
        }
        found
    }

    fn expect_punct(&mut self, punct: Punct) -> ParseResult<()> {
        if self.eat_punct(punct) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn keyword_at(&self, ahead: usize, keyword: &str) -> bool {
        matches!(
            self.kind_at(ahead),
            Some(TokenKind::Word { text, quoted: false }) if text.eq_ignore_ascii_case(keyword)
        )
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        self.keyword_at(0, keyword)
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        let found = self.at_keyword(keyword);
        if found {
            self.pos += 1;
        }
        found
    }

    fn eat_any_keyword(&mut self, keywords: &[&str]) -> bool {
        keywords.iter().any(|keyword| self.eat_keyword(keyword))
    }

    fn expect_keyword(&mut self, keyword: &str) -> ParseResult<()> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn name(&mut self) -> ParseResult<Name> {
        match self.kind_at(0) {
            Some(TokenKind::Word { text, .. }) => {
                let name = Name::new(text.as_str());
                self.pos += 1;
                Ok(name)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn deepen(&mut self) -> ParseResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SyntaxError::new(
                self.offset(),
                SyntaxErrorKind::NestingTooDeep,
            ));
        }
        Ok(())
    }

    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> ParseResult<T>) -> ParseResult<T> {
        let before = self.depth;
        self.deepen()?;
        let result = parse(self);
        self.depth = before;
        result
    }
}

#[cfg(test)]
#[path = "parser.test.rs"]
mod tests;
