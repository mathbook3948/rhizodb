use super::ParseResult;
use super::Parser;
use crate::gql::ast::TypeExpr;
use crate::gql::ast::TypeMember;
use crate::gql::ast::ValueType;
use crate::gql::lexer::Punct;

const NAMES: [(&[&str], ValueType); 32] = [
    (&["BOOLEAN"], ValueType::Boolean),
    (&["BOOL"], ValueType::Boolean),
    (&["STRING"], ValueType::String),
    (&["VARCHAR"], ValueType::String),
    (&["INTEGER"], ValueType::Integer),
    (&["INT"], ValueType::Integer),
    (&["INT64"], ValueType::Integer),
    (&["INTEGER64"], ValueType::Integer),
    (&["SIGNED", "INTEGER"], ValueType::Integer),
    (&["FLOAT"], ValueType::Float),
    (&["FLOAT64"], ValueType::Float),
    (&["DOUBLE"], ValueType::Float),
    (&["DATE"], ValueType::Date),
    (&["LOCAL", "TIME"], ValueType::LocalTime),
    (&["TIME", "WITHOUT", "TIME", "ZONE"], ValueType::LocalTime),
    (&["ZONED", "TIME"], ValueType::ZonedTime),
    (&["TIME", "WITH", "TIME", "ZONE"], ValueType::ZonedTime),
    (&["LOCAL", "DATETIME"], ValueType::LocalDateTime),
    (
        &["TIMESTAMP", "WITHOUT", "TIME", "ZONE"],
        ValueType::LocalDateTime,
    ),
    (&["ZONED", "DATETIME"], ValueType::ZonedDateTime),
    (
        &["TIMESTAMP", "WITH", "TIME", "ZONE"],
        ValueType::ZonedDateTime,
    ),
    (&["DURATION"], ValueType::Duration),
    (&["BYTES"], ValueType::Bytes),
    (&["MAP"], ValueType::Map),
    (&["RECORD"], ValueType::Map),
    (&["NODE"], ValueType::Node),
    (&["VERTEX"], ValueType::Node),
    (&["RELATIONSHIP"], ValueType::Relationship),
    (&["EDGE"], ValueType::Relationship),
    (&["PATH"], ValueType::Path),
    (&["NULL"], ValueType::Null),
    (&["NOTHING"], ValueType::Nothing),
];

impl Parser {
    pub(super) fn type_expr(&mut self) -> ParseResult<TypeExpr> {
        let pipes = self.label_pipe_allowed();
        let mut members = self.nested(Self::type_term)?;
        while pipes && self.at_punct(Punct::Pipe) && !self.punct_at(1, Punct::Pipe) {
            self.pos += 1;
            members.extend(self.nested(Self::type_term)?);
        }
        Ok(TypeExpr { members })
    }

    fn type_term(&mut self) -> ParseResult<Vec<TypeMember>> {
        let mut members = if self.at_keyword("ANY") && self.punct_at(1, Punct::Lt) {
            self.pos += 2;
            let inner = self.type_expr()?;
            self.expect_punct(Punct::Gt)?;
            inner.members
        } else {
            vec![TypeMember {
                value_type: self.value_type()?,
                not_null: false,
            }]
        };
        while self.at_keyword("LIST") || self.at_keyword("ARRAY") {
            self.pos += 1;
            members = vec![TypeMember {
                value_type: ValueType::List(Box::new(TypeExpr { members })),
                not_null: false,
            }];
        }
        if self.at_keyword("NOT") && self.keyword_at(1, "NULL") {
            self.pos += 2;
            for member in &mut members {
                member.not_null = true;
            }
        }
        Ok(members)
    }

    pub(super) fn value_type(&mut self) -> ParseResult<ValueType> {
        if (self.at_keyword("LIST") || self.at_keyword("ARRAY")) && self.punct_at(1, Punct::Lt) {
            self.pos += 2;
            let inner = self.type_expr()?;
            self.expect_punct(Punct::Gt)?;
            return Ok(ValueType::List(Box::new(inner)));
        }
        if self.eat_keyword("ANY") {
            if self.eat_keyword("NODE") || self.eat_keyword("VERTEX") {
                return Ok(ValueType::Node);
            }
            if self.eat_keyword("EDGE") || self.eat_keyword("RELATIONSHIP") {
                return Ok(ValueType::Relationship);
            }
            self.eat_keyword("VALUE");
            return Ok(ValueType::Any);
        }
        for (words, value_type) in NAMES {
            if words
                .iter()
                .enumerate()
                .all(|(ahead, word)| self.keyword_at(ahead, word))
            {
                self.pos += words.len();
                return Ok(value_type);
            }
        }
        Err(self.unexpected())
    }
}
