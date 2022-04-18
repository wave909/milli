//! BNF grammar:
//!
//! ```text
//! condition      = value ("==" | ">" ...) value
//! to             = value value TO value
//! ```

use nom::branch::alt;
use nom::bytes::complete::tag;
use nom::combinator::cut;
use nom::sequence::tuple;
use Condition::*;

use crate::{parse_value, FilterCondition, IResult, Span, Token};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition<'a> {
    GreaterThan(Token<'a>),
    GreaterThanOrEqual(Token<'a>),
    Equal(Token<'a>),
    NotEqual(Token<'a>),
    LowerThan(Token<'a>),
    Includes(Token<'a>),
    NotIncludes(Token<'a>),
    LowerThanOrEqual(Token<'a>),
    Between { from: Token<'a>, to: Token<'a> },
}

impl<'a> Condition<'a> {
    /// This method can return two operations in case it must express
    /// an OR operation for the between case (i.e. `TO`).
    pub fn negate(self) -> (Self, Option<Self>) {
        match self {
            GreaterThan(n) => (LowerThanOrEqual(n), None),
            GreaterThanOrEqual(n) => (LowerThan(n), None),
            Equal(s) => (NotEqual(s), None),
            NotEqual(s) => (Equal(s), None),
            Includes(s) => (NotIncludes(s), None),
            NotIncludes(s) => (Includes(s), None),
            LowerThan(n) => (GreaterThanOrEqual(n), None),
            LowerThanOrEqual(n) => (GreaterThan(n), None),
            Between { from, to } => (LowerThan(from), Some(GreaterThan(to))),
        }
    }
}

/// condition      = value ("==" | ">" ...) value
pub fn parse_condition(input: Span) -> IResult<FilterCondition> {
    let operator = alt((tag("<="), tag(">="), tag("!="), tag("<"), tag(">"), tag("="), tag("*"), tag("!*")));
    let (input, (fid, op, value)) = tuple((parse_value, operator, cut(parse_value)))(input)?;

    let condition = match *op.fragment() {
        "<=" => FilterCondition::Condition { fid, op: LowerThanOrEqual(value) },
        ">=" => FilterCondition::Condition { fid, op: GreaterThanOrEqual(value) },
        "!=" => FilterCondition::Condition { fid, op: NotEqual(value) },
        "<" => FilterCondition::Condition { fid, op: LowerThan(value) },
        ">" => FilterCondition::Condition { fid, op: GreaterThan(value) },
        "=" => FilterCondition::Condition { fid, op: Equal(value) },
        "*" => FilterCondition::Condition { fid, op: Includes(value) },
        "!*" => FilterCondition::Condition { fid, op: NotIncludes(value) },
        _ => unreachable!(),
    };

    Ok((input, condition))
}

/// to             = value value TO value
pub fn parse_to(input: Span) -> IResult<FilterCondition> {
    let (input, (key, from, _, to)) =
        tuple((parse_value, parse_value, tag("TO"), cut(parse_value)))(input)?;

    Ok((input, FilterCondition::Condition { fid: key, op: Between { from, to } }))
}
