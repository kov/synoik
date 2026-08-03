use std::str::FromStr;

use regex::Regex;

mod merge_with;
pub use merge_with::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percent(pub f64);

// MIN and MAX generics are only used during parsing to check the value.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FloatOrInt<const MIN: i32, const MAX: i32>(pub f64);

/// Flag, with an optional explicit value.
///
/// Intended to be used as an `Option<MaybeBool>` field, as a tri-state:
/// - (missing): unset, `None`
/// - just `field`: set, `Some(true)`
/// - explicitly `field true` or `field false`: set, `Some(true)` or `Some(false)`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag(pub bool);

/// `Regex` that implements `PartialEq` by its string form.
#[derive(Debug, Clone)]
pub struct RegexEq(pub Regex);

impl PartialEq for RegexEq {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for RegexEq {}

impl FromStr for RegexEq {
    type Err = <Regex as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Regex::from_str(s).map(Self)
    }
}

impl<const MIN: i32, const MAX: i32> MergeWith<FloatOrInt<MIN, MAX>> for f64 {
    fn merge_with(&mut self, part: &FloatOrInt<MIN, MAX>) {
        *self = part.0;
    }
}

impl MergeWith<Flag> for bool {
    fn merge_with(&mut self, part: &Flag) {
        *self = part.0;
    }
}
