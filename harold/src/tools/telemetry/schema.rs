//! A declaration: what the store keeps for one name.

use std::collections::BTreeMap;

use super::name::{
    Attr, Kind, MAX_MEANING, MAX_SERIES, Refused, check_keys, parse_name, series_count,
};

/// A gauge's last values, by series key (see `series_key`).
pub(crate) type LastValues = BTreeMap<String, f64>;

/// The key of one series: its `key=value` pairs, sorted and joined by a comma.
/// A series with no attributes has the empty key. A declared key or value
/// holds no `,` and no `=`, so the key is not ambiguous.
pub(crate) fn series_key(pairs: &[(String, String)]) -> String {
    let mut parts: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.sort();
    parts.join(",")
}

/// One declared name. `last_values` holds a gauge's last value for each
/// series; `put` ignores it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declaration {
    pub name: String,
    pub kind: Kind,
    pub meaning: String,
    pub attrs: Vec<Attr>,
    pub created_ms: i64,
    pub last_values: LastValues,
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
