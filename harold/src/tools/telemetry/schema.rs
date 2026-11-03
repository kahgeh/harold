//! A declaration: what the store keeps for one name.

use super::name::{
    Attr, Kind, MAX_MEANING, MAX_SERIES, Refused, check_keys, parse_name, series_count,
};

/// One declared name. `last_value` is a gauge's last value; `put` ignores it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declaration {
    pub name: String,
    pub kind: Kind,
    pub meaning: String,
    pub attrs: Vec<Attr>,
    pub created_ms: i64,
    pub last_value: Option<f64>,
}

impl Declaration {
    /// Checks the name, the meaning, the keys and, for a counter or a gauge,
    /// the series. The first failure is the `Err`.
    pub(crate) fn validate(&self) -> Result<(), Refused> {
        parse_name(&self.name)?;
        let length = self.meaning.chars().count();
        if length > MAX_MEANING {
            return Err(Refused::MeaningTooLong { length });
        }
        check_keys(&self.attrs)?;
        let series = series_count(&self.attrs);
        if self.kind != Kind::Event && series > MAX_SERIES {
            return Err(Refused::TooManySeries {
                name: self.name.clone(),
                series,
            });
        }
        Ok(())
    }

    /// Whether the other declaration has the same name, kind and attribute
    /// lists. The meaning and the time do not count. Key order and value order
    /// do not count either.
    pub(crate) fn same_shape(&self, other: &Declaration) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && shape(&self.attrs) == shape(&other.attrs)
    }
}

fn shape(attrs: &[Attr]) -> Vec<Attr> {
    let mut sorted: Vec<Attr> = attrs.to_vec();
    for attr in &mut sorted {
        if let super::name::ValueList::Values(values) = &mut attr.values {
            values.sort();
        }
    }
    sorted.sort_by(|a, b| a.key.cmp(&b.key));
    sorted
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
