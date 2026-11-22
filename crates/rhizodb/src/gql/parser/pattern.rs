use super::ParseResult;
use super::Parser;
use crate::gql::ast::Direction;
use crate::gql::ast::Expr;
use crate::gql::ast::Length;
use crate::gql::ast::Name;
use crate::gql::ast::NodePattern;
use crate::gql::ast::PatternElement;
use crate::gql::ast::PatternPart;
use crate::gql::ast::PatternStep;
use crate::gql::ast::Properties;
use crate::gql::ast::RelationshipPattern;
use crate::gql::lexer::Punct;
use crate::gql::lexer::TokenKind;

impl Parser {
    pub(super) fn pattern_parts(&mut self) -> ParseResult<Vec<PatternPart>> {
        self.comma_separated(Self::pattern_part)
    }

    pub(super) fn pattern_part(&mut self) -> ParseResult<PatternPart> {
        let path_variable = if self.at_word() && self.punct_at(1, Punct::Eq) {
            let name = self.name()?;
            self.pos += 1;
            Some(name)
        } else {
            None
        };
        if self.at_word() && self.punct_at(1, Punct::LParen) {
            return Err(self.unsupported());
        }
        let element = self.pattern_element()?;
        Ok(PatternPart {
            path_variable,
            element,
        })
    }

    pub(super) fn pattern_element(&mut self) -> ParseResult<PatternElement> {
        let start = self.node_pattern()?;
        let mut steps = Vec::new();
        while self.at_punct(Punct::Minus) || self.at_punct(Punct::Lt) {
            let relationship = self.relationship_pattern()?;
            let node = self.node_pattern()?;
            steps.push(PatternStep { relationship, node });
        }
        Ok(PatternElement { start, steps })
    }

    pub(super) fn map_entries(&mut self) -> ParseResult<Vec<(Name, Expr)>> {
        self.expect_punct(Punct::LBrace)?;
        if self.eat_punct(Punct::RBrace) {
            return Ok(Vec::new());
        }
        let entries = self.comma_separated(|parser| {
            let key = parser.name()?;
            parser.expect_punct(Punct::Colon)?;
            let value = parser.expression()?;
            Ok((key, value))
        })?;
        self.expect_punct(Punct::RBrace)?;
        Ok(entries)
    }

    pub(super) fn at_word(&self) -> bool {
        matches!(self.kind_at(0), Some(TokenKind::Word { .. }))
    }

    fn node_pattern(&mut self) -> ParseResult<NodePattern> {
        self.expect_punct(Punct::LParen)?;
        let variable = if self.at_word() {
            Some(self.name()?)
        } else {
            None
        };
        let labels = self.label_list()?;
        let properties = self.properties()?;
        self.expect_punct(Punct::RParen)?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
        })
    }

    fn properties(&mut self) -> ParseResult<Option<Properties>> {
        if self.at_punct(Punct::LBrace) {
            return Ok(Some(Properties::Map(self.map_entries()?)));
        }
        if let Some(TokenKind::Parameter(name)) = self.kind_at(0) {
            let name = Name::new(name.as_str());
            self.pos += 1;
            return Ok(Some(Properties::Parameter(name)));
        }
        Ok(None)
    }

    fn relationship_pattern(&mut self) -> ParseResult<RelationshipPattern> {
        let points_left = self.eat_punct(Punct::Lt);
        self.expect_punct(Punct::Minus)?;
        let mut relationship = RelationshipPattern {
            variable: None,
            types: Vec::new(),
            length: None,
            properties: None,
            direction: Direction::Undirected,
        };
        let points_right = if self.eat_punct(Punct::LBracket) {
            self.relationship_detail(&mut relationship)?;
            self.expect_punct(Punct::RBracket)?;
            self.expect_punct(Punct::Minus)?;
            self.eat_punct(Punct::Gt)
        } else {
            self.eat_punct(Punct::Minus);
            self.eat_punct(Punct::Gt)
        };
        relationship.direction = match (points_left, points_right) {
            (true, false) => Direction::Incoming,
            (false, true) => Direction::Outgoing,
            _ => Direction::Undirected,
        };
        Ok(relationship)
    }

    fn relationship_detail(&mut self, relationship: &mut RelationshipPattern) -> ParseResult<()> {
        if self.at_word() {
            relationship.variable = Some(self.name()?);
        }
        if self.eat_punct(Punct::Colon) {
            relationship.types.push(self.name()?);
            while self.eat_punct(Punct::Pipe) {
                self.eat_punct(Punct::Colon);
                relationship.types.push(self.name()?);
            }
        }
        if self.eat_punct(Punct::Star) {
            relationship.length = Some(self.length_bounds());
        }
        relationship.properties = self.properties()?;
        Ok(())
    }

    fn length_bounds(&mut self) -> Length {
        let min = self.integer_bound();
        if self.eat_punct(Punct::DotDot) {
            Length {
                min,
                max: self.integer_bound(),
            }
        } else {
            Length { min, max: min }
        }
    }

    fn integer_bound(&mut self) -> Option<u64> {
        if let Some(TokenKind::Integer(bound)) = self.kind_at(0) {
            let bound = *bound;
            self.pos += 1;
            Some(bound)
        } else {
            None
        }
    }

    pub(super) fn relationship_starts_at(&self, index: usize) -> bool {
        let kind_at = |ahead: usize| self.tokens.get(index + ahead).map(|token| &token.kind);
        let is = |ahead: usize, punct: Punct| kind_at(ahead) == Some(&TokenKind::Punct(punct));
        let touches_next = |ahead: usize| {
            matches!(
                (self.tokens.get(index + ahead), self.tokens.get(index + ahead + 1)),
                (Some(left), Some(right)) if left.end == right.start
            )
        };
        if is(0, Punct::Minus) {
            return is(1, Punct::LBracket)
                || (touches_next(0) && (is(1, Punct::Minus) || is(1, Punct::Gt)));
        }
        is(0, Punct::Lt)
            && is(1, Punct::Minus)
            && touches_next(0)
            && (is(2, Punct::Minus) || is(2, Punct::LBracket))
    }
}

#[cfg(test)]
#[path = "pattern.test.rs"]
mod tests;
