use super::ParseResult;
use super::Parser;
use crate::gql::ast::Direction;
use crate::gql::ast::Expr;
use crate::gql::ast::Length;
use crate::gql::ast::Name;
use crate::gql::ast::NodePattern;
use crate::gql::ast::PathMode;
use crate::gql::ast::PathSearch;
use crate::gql::ast::PathSelector;
use crate::gql::ast::PatternElement;
use crate::gql::ast::PatternPart;
use crate::gql::ast::PatternStep;
use crate::gql::ast::Properties;
use crate::gql::ast::QuantifiedPattern;
use crate::gql::ast::RelationshipPattern;
use crate::gql::error::SyntaxError;
use crate::gql::error::SyntaxErrorKind;
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
        if let Some(selector) = self.cypher_shortest() {
            self.pos += 2;
            let element = self.pattern_element()?;
            self.expect_punct(Punct::RParen)?;
            return Ok(PatternPart {
                path_variable,
                search: Some(PathSearch {
                    selector: Some(selector),
                    mode: None,
                    cypher: true,
                }),
                element,
            });
        }
        let search = self.path_search()?;
        if self.at_word() && self.punct_at(1, Punct::LParen) {
            return Err(self.unsupported());
        }
        let element = self.pattern_element()?;
        Ok(PatternPart {
            path_variable,
            search,
            element,
        })
    }

    fn cypher_shortest(&self) -> Option<PathSelector> {
        if !self.punct_at(1, Punct::LParen) {
            return None;
        }
        if self.at_keyword("shortestPath") {
            Some(PathSelector::AnyShortest)
        } else if self.at_keyword("allShortestPaths") {
            Some(PathSelector::AllShortest)
        } else {
            None
        }
    }

    fn path_search(&mut self) -> ParseResult<Option<PathSearch>> {
        let selector = if self.eat_keyword("ALL") {
            Some(if self.eat_keyword("SHORTEST") {
                PathSelector::AllShortest
            } else {
                PathSelector::All
            })
        } else if self.eat_keyword("ANY") {
            Some(if self.eat_keyword("SHORTEST") {
                PathSelector::AnyShortest
            } else {
                PathSelector::Any(self.path_count()?.unwrap_or(1))
            })
        } else if self.eat_keyword("SHORTEST") {
            Some(PathSelector::Shortest(0))
        } else {
            None
        };
        let count = match selector {
            Some(PathSelector::Shortest(_)) => self.path_count()?,
            _ => None,
        };
        let mode = self.path_mode();
        if (selector.is_some() || mode.is_some()) && !self.eat_keyword("PATH") {
            self.eat_keyword("PATHS");
        }
        let selector = match selector {
            Some(PathSelector::Shortest(_)) => {
                if self.eat_any_keyword(&["GROUP", "GROUPS"]) {
                    Some(PathSelector::ShortestGroups(count.unwrap_or(1)))
                } else {
                    match count {
                        Some(count) => Some(PathSelector::Shortest(count)),
                        None => return Err(self.unexpected()),
                    }
                }
            }
            other => other,
        };
        Ok(
            (selector.is_some() || mode.is_some()).then_some(PathSearch {
                selector,
                mode,
                cypher: false,
            }),
        )
    }

    fn path_count(&mut self) -> ParseResult<Option<u64>> {
        Ok(match self.kind_at(0) {
            Some(TokenKind::Integer(count)) => {
                let count = *count;
                self.pos += 1;
                Some(count)
            }
            _ => None,
        })
    }

    fn path_mode(&mut self) -> Option<PathMode> {
        [
            ("WALK", PathMode::Walk),
            ("TRAIL", PathMode::Trail),
            ("SIMPLE", PathMode::Simple),
            ("ACYCLIC", PathMode::Acyclic),
        ]
        .into_iter()
        .find_map(|(keyword, mode)| self.eat_keyword(keyword).then_some(mode))
    }

    pub(super) fn pattern_element(&mut self) -> ParseResult<PatternElement> {
        self.element(true)
    }

    fn element(&mut self, groups: bool) -> ParseResult<PatternElement> {
        let start = if self.at_group() {
            NodePattern::anonymous()
        } else {
            self.node_pattern()?
        };
        let mut steps = Vec::new();
        loop {
            if self.at_group() {
                if !groups {
                    return Err(self.unsupported());
                }
                let group = Box::new(self.quantified_pattern()?);
                let node = if self.at_punct(Punct::LParen) && !self.at_group() {
                    self.node_pattern()?
                } else {
                    NodePattern::anonymous()
                };
                steps.push(PatternStep::Group { group, node });
            } else if [Punct::Minus, Punct::Lt, Punct::Tilde]
                .iter()
                .any(|punct| self.at_punct(*punct))
            {
                let mut relationship = self.relationship_pattern()?;
                if let Some(length) = self.quantifier()? {
                    if relationship.length.is_some() {
                        return Err(self.invalid_relationship_pattern());
                    }
                    relationship.length = Some(length);
                }
                let node = self.node_pattern()?;
                steps.push(PatternStep::Edge { relationship, node });
            } else {
                return Ok(PatternElement { start, steps });
            }
        }
    }

    fn at_group(&self) -> bool {
        self.at_punct(Punct::LParen) && self.punct_at(1, Punct::LParen)
    }

    fn quantified_pattern(&mut self) -> ParseResult<QuantifiedPattern> {
        self.expect_punct(Punct::LParen)?;
        let element = self.nested(|parser| parser.element(false))?;
        let filter = if self.eat_keyword("WHERE") {
            Some(self.predicate()?)
        } else {
            None
        };
        self.expect_punct(Punct::RParen)?;
        let Some(length) = self.quantifier()? else {
            return Err(self.unsupported());
        };
        Ok(QuantifiedPattern {
            element,
            filter,
            min: length.min.unwrap_or(0),
            max: length.max,
        })
    }

    fn quantifier(&mut self) -> ParseResult<Option<Length>> {
        if self.eat_punct(Punct::Star) {
            return Ok(Some(Length {
                min: Some(0),
                max: None,
            }));
        }
        if self.eat_punct(Punct::Plus) {
            return Ok(Some(Length {
                min: Some(1),
                max: None,
            }));
        }
        if !self.at_punct(Punct::LBrace) {
            return Ok(None);
        }
        self.pos += 1;
        let min = self.path_count()?;
        let max = if self.eat_punct(Punct::Comma) {
            self.path_count()?
        } else if min.is_some() {
            min
        } else {
            return Err(self.invalid_relationship_pattern());
        };
        self.expect_punct(Punct::RBrace)?;
        if let (Some(min), Some(max)) = (min, max)
            && min > max
        {
            return Err(self.invalid_relationship_pattern());
        }
        Ok(Some(Length {
            min: Some(min.unwrap_or(0)),
            max,
        }))
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
        let variable = self.element_variable()?;
        let labels = self.pattern_labels(true)?;
        let properties = self.properties()?;
        self.expect_punct(Punct::RParen)?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
        })
    }

    fn element_variable(&mut self) -> ParseResult<Option<Name>> {
        let variable_named_is = self.keyword_at(1, "IS") && self.label_starts_at(2);
        let starts_labels = self.at_keyword("IS") && !variable_named_is && self.label_starts_at(1);
        if self.at_word() && !starts_labels {
            Ok(Some(self.name()?))
        } else {
            Ok(None)
        }
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
        let undirected = self.eat_punct(Punct::Tilde);
        if !undirected {
            self.expect_punct(Punct::Minus)?;
        }
        let mut relationship = RelationshipPattern {
            variable: None,
            labels: None,
            length: None,
            properties: None,
            direction: Direction::Any,
        };
        if self.eat_punct(Punct::LBracket) {
            self.relationship_detail(&mut relationship)?;
            self.expect_punct(Punct::RBracket)?;
            self.expect_punct(if undirected {
                Punct::Tilde
            } else {
                Punct::Minus
            })?;
        } else if !undirected {
            self.eat_punct(Punct::Minus);
        }
        let arrow = self.offset();
        let points_right = self.eat_punct(Punct::Gt);
        relationship.direction = match (undirected, points_left, points_right) {
            (false, false, true) => Direction::Outgoing,
            (false, true, false) => Direction::Incoming,
            (false, true, true) => Direction::IncomingOrOutgoing,
            (false, false, false) => Direction::Any,
            (true, false, false) => Direction::Undirected,
            (true, true, false) => Direction::IncomingOrUndirected,
            (true, false, true) => Direction::UndirectedOrOutgoing,
            (true, true, true) => {
                return Err(SyntaxError::new(arrow, SyntaxErrorKind::UnexpectedToken));
            }
        };
        Ok(relationship)
    }

    fn relationship_detail(&mut self, relationship: &mut RelationshipPattern) -> ParseResult<()> {
        relationship.variable = self.element_variable()?;
        relationship.labels = self.pattern_labels(false)?;
        if self.eat_punct(Punct::Star) {
            relationship.length = Some(self.length_bounds()?);
        } else if self.at_punct(Punct::DotDot)
            || matches!(self.kind_at(0), Some(TokenKind::Integer(_)))
        {
            return Err(self.invalid_relationship_pattern());
        }
        relationship.properties = self.properties()?;
        Ok(())
    }

    fn length_bounds(&mut self) -> ParseResult<Length> {
        let min = self.integer_bound()?;
        Ok(if self.eat_punct(Punct::DotDot) {
            Length {
                min,
                max: self.integer_bound()?,
            }
        } else {
            Length { min, max: min }
        })
    }

    fn integer_bound(&mut self) -> ParseResult<Option<u64>> {
        if self.at_punct(Punct::Minus) {
            return Err(self.invalid_relationship_pattern());
        }
        Ok(if let Some(TokenKind::Integer(bound)) = self.kind_at(0) {
            let bound = *bound;
            self.pos += 1;
            Some(bound)
        } else {
            None
        })
    }

    fn invalid_relationship_pattern(&self) -> SyntaxError {
        SyntaxError::new(self.offset(), SyntaxErrorKind::InvalidRelationshipPattern)
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
        if is(0, Punct::Tilde) || (is(0, Punct::Lt) && is(1, Punct::Tilde)) {
            return true;
        }
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
