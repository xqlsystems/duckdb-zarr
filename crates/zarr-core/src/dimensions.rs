//! Parsed axis names for a single selected array.
//!
//! Positional names are checked against the selected rank and owned by a private
//! type. Declared names retain the existing metadata semantics without these
//! checks; the enum records a caller-established distinction, not a validity proof.
//! The checked type is consumed at table construction, not propagated through Table.

use std::collections::HashSet;

use crate::meta::synthesize_dim_names;
use crate::tree::fold_case;

/// Stored names may bind coordinates; positional names must remain indices.
#[derive(Debug)]
pub(crate) enum ArrayDimensions {
    Declared(Vec<String>),
    Positional(PositionalDimensions),
}

/// Names parsed for an unnamed array; the private field prevents unchecked construction.
#[derive(Debug)]
pub(crate) struct PositionalDimensions(Vec<String>);

impl ArrayDimensions {
    /// Preserve declared names, or parse positional names for the selected rank.
    pub(crate) fn parse(
        array_name: &str,
        ndim: usize,
        declared: Option<Vec<String>>,
        supplied: Option<&[String]>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        match declared {
            Some(names) => Ok(Self::Declared(names)),
            None => Ok(Self::Positional(PositionalDimensions::parse(
                array_name, ndim, supplied,
            )?)),
        }
    }
}

impl PositionalDimensions {
    /// Parse rank-matching, non-empty, NUL-free, case-insensitively unique names.
    fn parse(
        array_name: &str,
        ndim: usize,
        supplied: Option<&[String]>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let Some(names) = supplied else {
            return Ok(Self(synthesize_dim_names(ndim)));
        };
        if names.len() != ndim {
            return Err(format!(
                "array '{array_name}' has {ndim} dimensions, but dims= supplies {} names",
                names.len()
            )
            .into());
        }
        let mut used = HashSet::new();
        for name in names {
            if name.trim().is_empty() || name.contains('\0') {
                return Err(
                    "dims= dimension names must be non-empty and contain no NUL characters".into(),
                );
            }
            if !used.insert(fold_case(name)) {
                return Err(format!(
                    "dims= dimension names must be distinct ignoring case: '{name}' is repeated"
                )
                .into());
            }
        }
        Ok(Self(names.to_vec()))
    }

    /// Materialize parsed names when constructing the generic table representation.
    pub(crate) fn into_names(self) -> Vec<String> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supplied_names_without_changing_spelling() {
        let names = vec!["cell ids".to_string(), "Gene \"names\"".to_string()];
        let parsed = ArrayDimensions::parse("plain", 2, None, Some(&names)).unwrap();
        let ArrayDimensions::Positional(parsed) = parsed else {
            panic!("supplied names must remain positional");
        };
        assert_eq!(parsed.into_names(), names);
    }

    #[test]
    fn rejects_invalid_supplied_names() {
        let cases = [
            (vec!["cells"], "2 dimensions"),
            (vec!["cells", "CELLS"], "distinct ignoring case"),
            (vec!["", "genes"], "non-empty"),
            (vec!["  ", "genes"], "non-empty"),
            (vec!["bad\0name", "genes"], "NUL"),
        ];
        for (names, message) in cases {
            let names = names.into_iter().map(str::to_string).collect::<Vec<_>>();
            let error = ArrayDimensions::parse("plain", 2, None, Some(&names))
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
        }
    }

    #[test]
    fn invalid_names_preserve_error_precedence() {
        let cases = [
            (
                2,
                vec![""],
                "array 'plain' has 2 dimensions, but dims= supplies 1 names",
            ),
            (
                2,
                vec!["", ""],
                "dims= dimension names must be non-empty and contain no NUL characters",
            ),
            (
                3,
                vec!["cells", "CELLS", ""],
                "dims= dimension names must be distinct ignoring case: 'CELLS' is repeated",
            ),
            (
                3,
                vec!["", "cells", "CELLS"],
                "dims= dimension names must be non-empty and contain no NUL characters",
            ),
        ];
        for (rank, names, expected) in cases {
            let names = names.into_iter().map(str::to_string).collect::<Vec<_>>();
            let error = ArrayDimensions::parse("plain", rank, None, Some(&names))
                .unwrap_err()
                .to_string();
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn declared_names_keep_their_provenance_and_ignore_fallbacks() {
        let fallbacks = [vec!["duplicate", "DUPLICATE"], vec!["wrong_length"]];
        for names in [vec!["row", "column"], vec!["dim_0", "dim_1"]] {
            let names = names.into_iter().map(str::to_string).collect::<Vec<_>>();
            for fallback in &fallbacks {
                let fallback = fallback
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>();
                let parsed =
                    ArrayDimensions::parse("plain", 2, Some(names.clone()), Some(&fallback))
                        .unwrap();
                let ArrayDimensions::Declared(parsed) = parsed else {
                    panic!(
                        "metadata names must remain declared, even when they look like placeholders"
                    );
                };
                assert_eq!(parsed, names);
            }
        }
    }

    #[test]
    fn defaults_and_scalar_axes_remain_positional() {
        for (rank, supplied, expected) in [
            (2, None, vec!["dim_0", "dim_1"]),
            (0, None, vec![]),
            (0, Some(Vec::<String>::new()), vec![]),
        ] {
            let parsed = ArrayDimensions::parse("plain", rank, None, supplied.as_deref()).unwrap();
            let ArrayDimensions::Positional(parsed) = parsed else {
                panic!("implicit axes must remain positional");
            };
            assert_eq!(parsed.into_names(), expected);
        }
    }

    #[test]
    fn parsed_names_own_their_validity() {
        let mut names = vec!["cells".to_string(), "genes".to_string()];
        let parsed = ArrayDimensions::parse("plain", 2, None, Some(&names)).unwrap();
        names[0].clear();
        let ArrayDimensions::Positional(parsed) = parsed else {
            panic!("supplied names must remain positional");
        };
        assert_eq!(parsed.into_names(), vec!["cells", "genes"]);
    }
}
