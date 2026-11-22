use super::ParseResult;
use super::Parser;
use crate::gql::ast::LabelExpr;
use crate::gql::error::SyntaxError;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::lexer::Punct;
use crate::gql::lexer::TokenKind;

impl Parser {
    /// The labels of a node or relationship pattern, after `IS` or `:`. Only a node pattern
    /// takes a Cypher list such as `:A:B`.
    pub(super) fn pattern_labels(&mut self, list: bool) -> ParseResult<Option<LabelExpr>> {
        if self.eat_keyword("IS") {
            return self.label_expression(true).map(Some);
        }
        if self.at_punct(Punct::Colon) {
            return self.colon_labels(list, true).map(Some);
        }
        Ok(None)
    }

    /// `:` and a label expression, or a Cypher list of plain names such as `:A:B`, which is
    /// their conjunction. Without `pipe`, a `|` ends the expression instead of joining it.
    pub(super) fn colon_labels(&mut self, list: bool, pipe: bool) -> ParseResult<LabelExpr> {
        self.expect_punct(Punct::Colon)?;
        let first = self.label_expression(pipe)?;
        if !(list && self.at_punct(Punct::Colon)) {
            return Ok(first);
        }
        let mut operands = vec![first];
        while self.at_punct(Punct::Colon) {
            let colon = self.offset();
            self.pos += 1;
            operands.push(self.label_expression(pipe)?);
            if !operands
                .iter()
                .all(|operand| matches!(operand, LabelExpr::Name(_)))
            {
                return Err(SyntaxError::new(colon, SyntaxErrorKind::UnexpectedToken));
            }
        }
        Ok(LabelExpr::And(operands))
    }

    /// Whether the token `ahead` can start a label expression.
    pub(super) fn label_starts_at(&self, ahead: usize) -> bool {
        matches!(self.kind_at(ahead), Some(TokenKind::Word { .. }))
            || [Punct::Bang, Punct::Percent, Punct::LParen]
                .iter()
                .any(|punct| self.punct_at(ahead, *punct))
    }

    /// A label expression. `|` joins the weakest, then `&`, then `!`.
    pub(super) fn label_expression(&mut self, pipe: bool) -> ParseResult<LabelExpr> {
        let mut operands = vec![self.label_term()?];
        while pipe && self.eat_punct(Punct::Pipe) {
            self.eat_punct(Punct::Colon);
            operands.push(self.label_term()?);
        }
        Ok(combine(operands, LabelExpr::Or))
    }

    fn label_term(&mut self) -> ParseResult<LabelExpr> {
        let mut operands = vec![self.label_factor()?];
        while self.eat_punct(Punct::Ampersand) {
            operands.push(self.label_factor()?);
        }
        Ok(combine(operands, LabelExpr::And))
    }

    fn label_factor(&mut self) -> ParseResult<LabelExpr> {
        if self.eat_punct(Punct::Bang) {
            return Ok(LabelExpr::Not(Box::new(self.label_primary()?)));
        }
        self.label_primary()
    }

    fn label_primary(&mut self) -> ParseResult<LabelExpr> {
        if self.eat_punct(Punct::Percent) {
            return Ok(LabelExpr::Wildcard);
        }
        if self.eat_punct(Punct::LParen) {
            let inner = self.nested(|parser| parser.label_expression(true))?;
            self.expect_punct(Punct::RParen)?;
            return Ok(inner);
        }
        Ok(LabelExpr::Name(self.name()?))
    }
}

fn combine(operands: Vec<LabelExpr>, join: fn(Vec<LabelExpr>) -> LabelExpr) -> LabelExpr {
    match <[LabelExpr; 1]>::try_from(operands) {
        Ok([only]) => only,
        Err(operands) => join(operands),
    }
}

#[cfg(test)]
#[path = "label.test.rs"]
mod tests;
