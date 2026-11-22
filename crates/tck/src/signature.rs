//! Reads the procedure signatures of `there exists a procedure` steps.

/// A procedure signature such as `test.my.proc(in :: INTEGER?) :: (out :: STRING?)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The qualified name, such as `test.my.proc`.
    pub name: String,
    /// Each parameter's name and declared type, such as `INTEGER?`.
    pub parameters: Vec<(String, String)>,
    /// Each output's name and declared type.
    pub outputs: Vec<(String, String)>,
}

/// Parses `name(parameters) :: (outputs)`, or returns why it cannot.
pub fn parse_signature(text: &str) -> Result<Signature, String> {
    let malformed = || format!("malformed procedure signature `{text}`");
    let (name, rest) = text.split_once('(').ok_or_else(malformed)?;
    let (parameters, rest) = rest.split_once(')').ok_or_else(malformed)?;
    let outputs = rest
        .trim()
        .strip_prefix("::")
        .map(str::trim)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.trim_end().strip_suffix(')'))
        .ok_or_else(malformed)?;
    Ok(Signature {
        name: name.trim().to_owned(),
        parameters: fields(parameters).ok_or_else(malformed)?,
        outputs: fields(outputs).ok_or_else(malformed)?,
    })
}

fn fields(list: &str) -> Option<Vec<(String, String)>> {
    if list.trim().is_empty() {
        return Some(Vec::new());
    }
    list.split(',')
        .map(|field| {
            let (name, kind) = field.split_once("::")?;
            Some((name.trim().to_owned(), kind.trim().to_owned()))
        })
        .collect()
}

#[cfg(test)]
#[path = "signature.test.rs"]
mod tests;
