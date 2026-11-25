use std::collections::BTreeMap;

use super::datum::Datum;
use super::eval::list_position;
use super::functions::Function;

/// Which parts of a value the check on the query text read as null in place of a value error.
/// A list taint only goes with a list value of the same length, a map taint holds only the
/// tainted entries of a map value, and each holds at least one tainted part, so a value without
/// any is always [`Taint::Clean`]. Evaluation outside the check only ever sees [`Taint::Clean`].
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Taint {
    #[default]
    Clean,
    Whole,
    List(Vec<Taint>),
    Map(BTreeMap<String, Taint>),
}

impl Taint {
    /// [`Taint::Whole`] when `tainted`, and [`Taint::Clean`] otherwise.
    pub(crate) fn when(tainted: bool) -> Taint {
        if tainted { Taint::Whole } else { Taint::Clean }
    }

    /// The taint of a list whose items have `items`.
    pub(crate) fn list(items: Vec<Taint>) -> Taint {
        if items.iter().any(Taint::is_tainted) {
            Taint::List(items)
        } else {
            Taint::Clean
        }
    }

    /// The taint of a map whose tainted entries are `entries`.
    pub(crate) fn map(entries: BTreeMap<String, Taint>) -> Taint {
        if entries.is_empty() {
            Taint::Clean
        } else {
            Taint::Map(entries)
        }
    }

    /// Whether any part of the value was read as null.
    pub(crate) fn is_tainted(&self) -> bool {
        !matches!(self, Taint::Clean)
    }

    /// Whether the value itself was read as null, so that even its type is unknown.
    pub(crate) fn is_whole(&self) -> bool {
        matches!(self, Taint::Whole)
    }

    /// The taint of a result that depends on every part of a value with this taint.
    pub(crate) fn flatten(&self) -> Taint {
        Taint::when(self.is_tainted())
    }

    /// The taints of the items of a list value with this taint, in order.
    pub(crate) fn items(self) -> impl Iterator<Item = Taint> {
        let (items, rest) = match self {
            Taint::List(items) => (items, Taint::Clean),
            other => (Vec::new(), other.flatten()),
        };
        items.into_iter().chain(std::iter::repeat(rest))
    }

    /// The taint of the entry under `key` of a map value with this taint.
    pub(crate) fn entry(&self, key: &str) -> Taint {
        match self {
            Taint::Map(entries) => entries.get(key).cloned().unwrap_or_default(),
            other => other.flatten(),
        }
    }

    /// The taint of what indexing a value of this taint by `index` reads: an item of a list or
    /// an entry of a map.
    pub(crate) fn at(&self, base: &Datum, index: &Datum) -> Taint {
        match (self, base, index) {
            (Taint::List(items), Datum::List(values), Datum::Integer(index)) => {
                list_position(values.len(), *index)
                    .and_then(|position| items.get(position).cloned())
                    .unwrap_or_default()
            }
            (Taint::Map(_), _, Datum::String(key)) => self.entry(key),
            (other, ..) => other.flatten(),
        }
    }
}

/// The taint of what `function` gives for `args`, whose taints are `taints`, and whether the
/// check of its argument types reads a part read as null. `size` and `keys` read only how many
/// items or which keys their argument has, `coalesce` reads whether each argument it passes over
/// is null, and the functions that return an item or a list of items keep the taint of each.
/// Without taints, as outside the check, the result is clean.
pub(crate) fn call(function: Function, args: &[Datum], taints: Vec<Taint>) -> (Taint, bool) {
    let first = taints.first().cloned().unwrap_or_default();
    let whole = first.is_whole();
    let arg = args.first().unwrap_or(&Datum::Null);
    match function {
        Function::Coalesce => {
            let mut passed_over = false;
            for (arg, taint) in args.iter().zip(taints) {
                if !arg.is_null() {
                    return (if passed_over { Taint::Whole } else { taint }, false);
                }
                passed_over |= taint.is_whole();
            }
            (Taint::when(passed_over), false)
        }
        Function::Size | Function::Keys => (Taint::when(whole), whole),
        Function::Properties => (first, whole),
        Function::Head => (first.at(arg, &Datum::Integer(0)), whole),
        Function::Last => (first.at(arg, &Datum::Integer(-1)), whole),
        Function::Tail => match first {
            Taint::List(items) => (Taint::list(items.into_iter().skip(1).collect()), false),
            other => (other.flatten(), whole),
        },
        Function::Reverse => match first {
            Taint::List(mut items) => {
                items.reverse();
                (Taint::list(items), false)
            }
            other => (other.flatten(), whole),
        },
        _ => {
            let tainted = taints.iter().any(Taint::is_tainted);
            (Taint::when(tainted), tainted)
        }
    }
}
