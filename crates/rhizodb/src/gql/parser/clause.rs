use super::ParseResult;
use super::Parser;
use crate::gql::ast::Call;
use crate::gql::ast::CallSubquery;
use crate::gql::ast::Clause;
use crate::gql::ast::Delete;
use crate::gql::ast::Expr;
use crate::gql::ast::Imports;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::Match;
use crate::gql::ast::Merge;
use crate::gql::ast::Name;
use crate::gql::ast::Projection;
use crate::gql::ast::ProjectionItem;
use crate::gql::ast::ProjectionItems;
use crate::gql::ast::RemoveItem;
use crate::gql::ast::SetItem;
use crate::gql::ast::SortDirection;
use crate::gql::ast::SortItem;
use crate::gql::ast::Span;
use crate::gql::ast::Unwind;
use crate::gql::ast::With;
use crate::gql::ast::Yield;
use crate::gql::ast::YieldItem;
use crate::gql::error::SyntaxError;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::lexer::Punct;
use crate::gql::lexer::TokenKind;

const CLAUSE_KEYWORDS: [&str; 17] = [
    "MATCH", "OPTIONAL", "INSERT", "CREATE", "RETURN", "WITH", "UNWIND", "SET", "REMOVE", "DELETE",
    "DETACH", "MERGE", "CALL", "FOREACH", "LOAD", "FILTER", "LET",
];

const UNSUPPORTED_CLAUSE_KEYWORDS: [&str; 2] = ["FOREACH", "LOAD"];

impl Parser {
    pub(super) fn at_clause_start(&self) -> bool {
        CLAUSE_KEYWORDS
            .iter()
            .any(|keyword| self.at_keyword(keyword))
    }

    pub(super) fn clause(&mut self) -> ParseResult<Clause> {
        let parse: fn(&mut Self) -> ParseResult<Clause> =
            if self.at_keyword("OPTIONAL") && self.keyword_at(1, "CALL") {
                Self::call_subquery
            } else if self.at_keyword("MATCH") || self.at_keyword("OPTIONAL") {
                Self::match_clause
            } else if self.eat_keyword("INSERT") || self.eat_keyword("CREATE") {
                Self::insert_clause
            } else if self.eat_keyword("MERGE") {
                Self::merge_clause
            } else if self.eat_keyword("RETURN") {
                Self::return_clause
            } else if self.eat_keyword("WITH") {
                Self::with_clause
            } else if self.eat_keyword("UNWIND") {
                Self::unwind_clause
            } else if self.eat_keyword("FILTER") {
                Self::filter_clause
            } else if self.eat_keyword("LET") {
                Self::let_clause
            } else if self.eat_keyword("SET") {
                Self::set_clause
            } else if self.eat_keyword("REMOVE") {
                Self::remove_clause
            } else if self.at_keyword("DELETE") || self.at_keyword("DETACH") {
                Self::delete_clause
            } else if self.at_keyword("CALL") {
                if self.punct_at(1, Punct::LBrace) || self.punct_at(1, Punct::LParen) {
                    Self::call_subquery
                } else {
                    self.pos += 1;
                    Self::call_clause
                }
            } else if UNSUPPORTED_CLAUSE_KEYWORDS
                .iter()
                .any(|keyword| self.at_keyword(keyword))
            {
                return Err(self.unsupported());
            } else {
                return Err(self.unexpected());
            };
        parse(self)
    }

    fn insert_clause(&mut self) -> ParseResult<Clause> {
        Ok(Clause::Insert(self.pattern_parts()?))
    }

    fn return_clause(&mut self) -> ParseResult<Clause> {
        Ok(Clause::Return(self.projection()?))
    }

    fn with_clause(&mut self) -> ParseResult<Clause> {
        let projection = self.projection()?;
        let filter = self.where_filter()?;
        Ok(Clause::With(With { projection, filter }))
    }

    fn unwind_clause(&mut self) -> ParseResult<Clause> {
        let expr = self.expression()?;
        self.expect_keyword("AS")?;
        let variable = self.name()?;
        Ok(Clause::Unwind(Unwind { expr, variable }))
    }

    fn filter_clause(&mut self) -> ParseResult<Clause> {
        self.eat_keyword("WHERE");
        Ok(Clause::Filter(self.predicate()?))
    }

    fn let_clause(&mut self) -> ParseResult<Clause> {
        Ok(Clause::Let(self.comma_separated(|parser| {
            let variable = parser.name()?;
            parser.expect_punct(Punct::Eq)?;
            Ok((variable, parser.expression()?))
        })?))
    }

    fn set_clause(&mut self) -> ParseResult<Clause> {
        Ok(Clause::Set(self.comma_separated(Self::set_item)?))
    }

    fn remove_clause(&mut self) -> ParseResult<Clause> {
        Ok(Clause::Remove(self.comma_separated(Self::remove_item)?))
    }

    fn delete_clause(&mut self) -> ParseResult<Clause> {
        let detach = self.eat_keyword("DETACH");
        self.expect_keyword("DELETE")?;
        let targets = self.comma_separated(Self::expression)?;
        Ok(Clause::Delete(Delete { detach, targets }))
    }

    pub(super) fn comma_separated<T>(
        &mut self,
        mut item: impl FnMut(&mut Self) -> ParseResult<T>,
    ) -> ParseResult<Vec<T>> {
        let mut items = vec![item(self)?];
        while self.eat_punct(Punct::Comma) {
            items.push(item(self)?);
        }
        Ok(items)
    }

    fn where_filter(&mut self) -> ParseResult<Option<Expr>> {
        if self.eat_keyword("WHERE") {
            Ok(Some(self.predicate()?))
        } else {
            Ok(None)
        }
    }

    fn match_clause(&mut self) -> ParseResult<Clause> {
        let optional = self.eat_keyword("OPTIONAL");
        self.expect_keyword("MATCH")?;
        let patterns = self.pattern_parts()?;
        let filter = self.where_filter()?;
        Ok(Clause::Match(Match {
            optional,
            patterns,
            filter,
        }))
    }

    fn call_clause(&mut self) -> ParseResult<Clause> {
        let mut name = vec![self.name()?];
        while self.eat_punct(Punct::Dot) {
            name.push(self.name()?);
        }
        let args = if self.eat_punct(Punct::LParen) {
            let args = if self.at_punct(Punct::RParen) {
                Vec::new()
            } else {
                self.comma_separated(Self::expression)?
            };
            self.expect_punct(Punct::RParen)?;
            Some(args)
        } else {
            None
        };
        let yields = if !self.eat_keyword("YIELD") {
            None
        } else if self.eat_punct(Punct::Star) {
            Some(Yield::Star)
        } else {
            let items = self.comma_separated(Self::yield_item)?;
            let filter = self.where_filter()?;
            Some(Yield::Items { items, filter })
        };
        Ok(Clause::Call(Call { name, args, yields }))
    }

    fn call_subquery(&mut self) -> ParseResult<Clause> {
        let optional = self.eat_keyword("OPTIONAL");
        self.expect_keyword("CALL")?;
        let imports = if self.eat_punct(Punct::LParen) {
            let imports = if self.eat_punct(Punct::Star) {
                Imports::All
            } else if self.at_punct(Punct::RParen) {
                Imports::Variables(Vec::new())
            } else {
                Imports::Variables(self.comma_separated(Self::name)?)
            };
            self.expect_punct(Punct::RParen)?;
            Some(imports)
        } else {
            None
        };
        self.expect_punct(Punct::LBrace)?;
        let body = self.nested(Self::next_query)?;
        self.expect_punct(Punct::RBrace)?;
        Ok(Clause::CallSubquery(Box::new(CallSubquery {
            optional,
            imports,
            body,
        })))
    }

    fn yield_item(&mut self) -> ParseResult<YieldItem> {
        let output = self.name()?;
        let alias = if self.eat_keyword("AS") {
            Some(self.name()?)
        } else {
            None
        };
        Ok(YieldItem { output, alias })
    }

    fn merge_clause(&mut self) -> ParseResult<Clause> {
        let pattern = self.pattern_part()?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.eat_keyword("ON") {
            let target = if self.eat_keyword("CREATE") {
                &mut on_create
            } else {
                self.expect_keyword("MATCH")?;
                &mut on_match
            };
            self.expect_keyword("SET")?;
            target.extend(self.comma_separated(Self::set_item)?);
        }
        Ok(Clause::Merge(Merge {
            pattern,
            on_create,
            on_match,
        }))
    }

    fn projection(&mut self) -> ParseResult<Projection> {
        let distinct = self.eat_keyword("DISTINCT");
        let items = if self.eat_punct(Punct::Star) {
            let extra = if self.eat_punct(Punct::Comma) {
                self.comma_separated(Self::projection_item)?
            } else {
                Vec::new()
            };
            ProjectionItems::Star(extra)
        } else {
            ProjectionItems::Items(self.comma_separated(Self::projection_item)?)
        };
        let order_by = if self.at_keyword("ORDER") {
            self.pos += 1;
            self.expect_keyword("BY")?;
            self.comma_separated(Self::sort_item)?
        } else {
            Vec::new()
        };
        let skip = if self.eat_keyword("SKIP") || self.eat_keyword("OFFSET") {
            Some(self.expression()?)
        } else {
            None
        };
        let limit = if self.eat_keyword("LIMIT") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(Projection {
            distinct,
            items,
            order_by,
            skip,
            limit,
        })
    }

    fn projection_item(&mut self) -> ParseResult<ProjectionItem> {
        let start = self.offset();
        let expr = self.expression()?;
        let span = Span {
            start,
            end: self.previous_end(),
        };
        let alias = if self.eat_keyword("AS") {
            Some(self.name()?)
        } else {
            None
        };
        Ok(ProjectionItem { expr, alias, span })
    }

    fn sort_item(&mut self) -> ParseResult<SortItem> {
        let expr = self.expression()?;
        let descending = self.eat_any_keyword(&["DESC", "DESCENDING"]);
        if !descending {
            self.eat_any_keyword(&["ASC", "ASCENDING"]);
        }
        let nulls_first = if self.eat_keyword("NULLS") {
            if self.eat_keyword("FIRST") {
                true
            } else {
                self.expect_keyword("LAST")?;
                false
            }
        } else {
            descending
        };
        let direction = match (descending, nulls_first) {
            (false, false) => SortDirection::Ascending,
            (true, true) => SortDirection::Descending,
            (false, true) => SortDirection::AscendingNullsFirst,
            (true, false) => SortDirection::DescendingNullsLast,
        };
        Ok(SortItem { expr, direction })
    }

    fn at_variable_followed_by(&self, punct: Punct) -> bool {
        matches!(self.kind_at(0), Some(TokenKind::Word { .. })) && self.punct_at(1, punct)
    }

    fn at_label_item(&self) -> bool {
        self.at_variable_followed_by(Punct::Colon)
            || (matches!(self.kind_at(0), Some(TokenKind::Word { .. }))
                && self.keyword_at(1, "IS")
                && self.label_starts_at(2)
                && !["NULL", "NOT", "LABELED"]
                    .iter()
                    .any(|keyword| self.keyword_at(2, keyword)))
    }

    fn label_item(&mut self) -> ParseResult<(Name, LabelExpr)> {
        let variable = self.name()?;
        match self.pattern_labels(true)? {
            Some(labels) => Ok((variable, labels)),
            None => Err(self.unexpected()),
        }
    }

    fn set_item(&mut self) -> ParseResult<SetItem> {
        if self.at_label_item() {
            let (variable, labels) = self.label_item()?;
            return Ok(SetItem::Labels { variable, labels });
        }
        if self.at_variable_followed_by(Punct::Eq) {
            let variable = self.name()?;
            self.pos += 1;
            let value = self.expression()?;
            return Ok(SetItem::ReplaceProperties { variable, value });
        }
        if self.at_variable_followed_by(Punct::PlusEq) {
            let variable = self.name()?;
            self.pos += 1;
            let value = self.expression()?;
            return Ok(SetItem::MergeProperties { variable, value });
        }
        let (base, key) = self.property_target()?;
        self.expect_punct(Punct::Eq)?;
        let value = self.expression()?;
        Ok(SetItem::Property { base, key, value })
    }

    fn remove_item(&mut self) -> ParseResult<RemoveItem> {
        if self.at_label_item() {
            let (variable, labels) = self.label_item()?;
            return Ok(RemoveItem::Labels { variable, labels });
        }
        let (base, key) = self.property_target()?;
        Ok(RemoveItem::Property { base, key })
    }

    fn property_target(&mut self) -> ParseResult<(Expr, Name)> {
        let start = self.offset();
        match self.postfix_expression()? {
            Expr::Property { base, key } => Ok((*base, key)),
            _ => Err(SyntaxError::new(start, SyntaxErrorKind::UnexpectedToken)),
        }
    }
}
