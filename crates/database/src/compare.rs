use collections::HashMap;
use serde::{Deserialize, Serialize};

use crate::{ResultSet, Value};

pub struct CompareSide<'a> {
    pub rows: &'a ResultSet,
    pub key: &'a [String],
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompareOptions {
    /// Pairs of `(left field, right field)` to compare. When empty, fields
    /// with the same name on both sides are compared.
    pub fields: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldDiff {
    pub left_field: String,
    pub right_field: String,
    pub left: Value,
    pub right: Value,
    /// The values are equal once representation is normalized (an integer
    /// and a numeric string, a DATETIME and epoch milliseconds, NULL and a
    /// missing field). Shown muted, apart from real differences.
    pub representation_only: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CompareRow {
    LeftOnly {
        key: Vec<Value>,
        row: usize,
    },
    RightOnly {
        key: Vec<Value>,
        row: usize,
    },
    Matched {
        key: Vec<Value>,
        left_row: usize,
        right_row: usize,
        diffs: Vec<FieldDiff>,
    },
}

impl CompareRow {
    pub fn has_real_difference(&self) -> bool {
        match self {
            CompareRow::Matched { diffs, .. } => diffs.iter().any(|diff| !diff.representation_only),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompareOutcome {
    pub rows: Vec<CompareRow>,
    pub compared_fields: Vec<(String, String)>,
    /// Keys appearing more than once on a side. Only the first row with each
    /// key is compared.
    pub duplicate_left_keys: usize,
    pub duplicate_right_keys: usize,
}

impl CompareOutcome {
    pub fn left_only(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, CompareRow::LeftOnly { .. }))
            .count()
    }

    pub fn right_only(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, CompareRow::RightOnly { .. }))
            .count()
    }

    pub fn changed(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, CompareRow::Matched { .. }) && row.has_real_difference())
            .count()
    }

    pub fn identical(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, CompareRow::Matched { diffs, .. } if diffs.is_empty()))
            .count()
    }
}

/// A keyed diff of two full result sets, which may come from different
/// engines. Both sides are held in memory and joined by hash.
pub fn compare(
    left: CompareSide,
    right: CompareSide,
    options: &CompareOptions,
) -> anyhow::Result<CompareOutcome> {
    anyhow::ensure!(!left.key.is_empty(), "choose a key on the left");
    anyhow::ensure!(
        left.key.len() == right.key.len(),
        "both sides need the same number of key fields"
    );
    let left_key = key_indices(left.rows, left.key)?;
    let right_key = key_indices(right.rows, right.key)?;

    let compared_fields = if options.fields.is_empty() {
        left.rows
            .columns
            .iter()
            .filter(|column| !left.key.contains(&column.name))
            .filter(|column| right.rows.column_index(&column.name).is_some())
            .map(|column| (column.name.clone(), column.name.clone()))
            .collect()
    } else {
        options.fields.clone()
    };
    let field_indices = compared_fields
        .iter()
        .map(|(left_field, right_field)| {
            Ok((
                field_index(left.rows, left_field)?,
                field_index(right.rows, right_field)?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut outcome = CompareOutcome {
        compared_fields: compared_fields.clone(),
        ..CompareOutcome::default()
    };
    let mut right_by_key: HashMap<Vec<String>, usize> = HashMap::default();
    for (index, row) in right.rows.rows.iter().enumerate() {
        if right_by_key
            .insert(canonical_key(row, &right_key), index)
            .is_some()
        {
            outcome.duplicate_right_keys += 1;
        }
    }
    // A later duplicate overwrote the first; keep the first instead.
    for (index, row) in right.rows.rows.iter().enumerate().rev() {
        right_by_key.insert(canonical_key(row, &right_key), index);
    }

    let mut matched_right = vec![false; right.rows.rows.len()];
    let mut seen_left: HashMap<Vec<String>, ()> = HashMap::default();
    for (left_index, row) in left.rows.rows.iter().enumerate() {
        let key = canonical_key(row, &left_key);
        if seen_left.insert(key.clone(), ()).is_some() {
            outcome.duplicate_left_keys += 1;
            continue;
        }
        let key_values = left_key.iter().map(|index| row[*index].clone()).collect();
        match right_by_key.get(&key) {
            Some(&right_index) => {
                matched_right[right_index] = true;
                let right_row = &right.rows.rows[right_index];
                let diffs = field_indices
                    .iter()
                    .zip(&compared_fields)
                    .filter_map(|((left_field, right_field), (left_name, right_name))| {
                        let left_value = &row[*left_field];
                        let right_value = &right_row[*right_field];
                        let representation_only = match equivalence(left_value, right_value) {
                            Equivalence::Same => return None,
                            Equivalence::Representation => true,
                            Equivalence::Different => false,
                        };
                        Some(FieldDiff {
                            left_field: left_name.clone(),
                            right_field: right_name.clone(),
                            left: left_value.clone(),
                            right: right_value.clone(),
                            representation_only,
                        })
                    })
                    .collect();
                outcome.rows.push(CompareRow::Matched {
                    key: key_values,
                    left_row: left_index,
                    right_row: right_index,
                    diffs,
                });
            }
            None => outcome.rows.push(CompareRow::LeftOnly {
                key: key_values,
                row: left_index,
            }),
        }
    }
    for (right_index, row) in right.rows.rows.iter().enumerate() {
        if !matched_right[right_index]
            && right_by_key.get(&canonical_key(row, &right_key)) == Some(&right_index)
        {
            outcome.rows.push(CompareRow::RightOnly {
                key: right_key.iter().map(|index| row[*index].clone()).collect(),
                row: right_index,
            });
        }
    }
    Ok(outcome)
}

fn key_indices(rows: &ResultSet, key: &[String]) -> anyhow::Result<Vec<usize>> {
    key.iter().map(|field| field_index(rows, field)).collect()
}

fn field_index(rows: &ResultSet, field: &str) -> anyhow::Result<usize> {
    rows.column_index(field)
        .ok_or_else(|| anyhow::anyhow!("no field named `{field}`"))
}

fn canonical_key(row: &[Value], indices: &[usize]) -> Vec<String> {
    indices
        .iter()
        .map(|index| key_string(&row[*index]))
        .collect()
}

fn key_string(value: &Value) -> String {
    // Integers keep their exact digits; going through f64 would merge ids
    // above 2^53.
    match value {
        Value::Int(value) => return value.to_string(),
        Value::Text(text) => {
            if let Ok(value) = text.trim().parse::<i64>() {
                return value.to_string();
            }
        }
        _ => {}
    }
    match canonical(value) {
        Canonical::Null => "\0null".to_string(),
        Canonical::Number(number) => format_number(number),
        Canonical::Text(text) => text,
        Canonical::Bool(value) => value.to_string(),
        Canonical::Other(text) => text,
    }
}

#[derive(Debug, PartialEq)]
enum Equivalence {
    Same,
    Representation,
    Different,
}

#[derive(Debug, PartialEq)]
enum Canonical {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Other(String),
}

fn canonical(value: &Value) -> Canonical {
    match value {
        Value::Missing | Value::Null => Canonical::Null,
        Value::Bool(value) => Canonical::Bool(*value),
        Value::Int(value) => Canonical::Number(*value as f64),
        Value::Float(value) => Canonical::Number(*value),
        Value::Text(text) => match text.trim().parse::<f64>() {
            Ok(number) if !text.trim().is_empty() && number.is_finite() => {
                Canonical::Number(number)
            }
            _ => Canonical::Text(text.clone()),
        },
        Value::Bytes(bytes) => Canonical::Other(format!("{bytes:?}")),
        Value::Json(json) => Canonical::Other(json.to_string()),
    }
}

fn equivalence(left: &Value, right: &Value) -> Equivalence {
    if left == right {
        return Equivalence::Same;
    }
    if let (Value::Int(left), Value::Int(right)) = (left, right) {
        // Exact, so large ids beyond 2^53 are not conflated.
        return if left == right {
            Equivalence::Same
        } else {
            Equivalence::Different
        };
    }
    if let (Value::Float(left), Value::Float(right)) = (left, right) {
        return if left == right {
            Equivalence::Same
        } else {
            Equivalence::Different
        };
    }
    let (left_canonical, right_canonical) = (canonical(left), canonical(right));
    if left_canonical == right_canonical {
        return Equivalence::Representation;
    }
    match (&left_canonical, &right_canonical) {
        (Canonical::Bool(flag), Canonical::Number(number))
        | (Canonical::Number(number), Canonical::Bool(flag))
            if (*number == 1.0 && *flag) || (*number == 0.0 && !*flag) =>
        {
            return Equivalence::Representation;
        }
        _ => {}
    }
    if let (Some(left_time), Some(right_time)) =
        (as_epoch_millis(left, right), as_epoch_millis(right, left))
        && (left_time - right_time).abs() < 1.0
    {
        return Equivalence::Representation;
    }
    if let (Value::Json(json), Value::Text(text)) | (Value::Text(text), Value::Json(json)) =
        (left, right)
        && serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .as_ref()
            == Some(json)
    {
        return Equivalence::Representation;
    }
    Equivalence::Different
}

/// Reads `value` as a point in time when it plausibly is one: a date string,
/// or a number when the other side is a date string (epoch ms or seconds).
fn as_epoch_millis(value: &Value, other: &Value) -> Option<f64> {
    if let Value::Text(text) = value
        && let Some(millis) = parse_datetime_millis(text)
    {
        return Some(millis);
    }
    let other_is_date = matches!(other, Value::Text(text) if parse_datetime_millis(text).is_some());
    if !other_is_date {
        return None;
    }
    let number = match value {
        Value::Int(value) => *value as f64,
        Value::Float(value) => *value,
        _ => return None,
    };
    // Ten digits of seconds cover 2001-2286; anything larger is milliseconds.
    Some(if number.abs() < 1e11 {
        number * 1000.0
    } else {
        number
    })
}

fn parse_datetime_millis(text: &str) -> Option<f64> {
    let text = text.trim();
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp_millis() as f64);
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(time.and_utc().timestamp_millis() as f64);
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|time| time.and_utc().timestamp_millis() as f64)
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 9.0e15 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Column;

    fn result_set(columns: &[&str], rows: Vec<Vec<Value>>) -> ResultSet {
        ResultSet {
            columns: columns
                .iter()
                .map(|name| Column {
                    name: name.to_string(),
                    type_name: String::new(),
                })
                .collect(),
            rows,
            truncated: false,
            affected_rows: None,
        }
    }

    #[test]
    fn test_compare_matches_across_representations() {
        // Ryse MySQL sellers vs Atlas Convex links: int ids vs float ids,
        // DATETIME vs epoch ms, NULL vs missing.
        let left = result_set(
            &["id", "email", "created", "phone"],
            vec![
                vec![
                    Value::Int(1),
                    Value::Text("a@x.com".into()),
                    Value::Text("2026-10-24 00:00:00".into()),
                    Value::Null,
                ],
                vec![
                    Value::Int(2),
                    Value::Text("b@x.com".into()),
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    Value::Int(3),
                    Value::Text("c@x.com".into()),
                    Value::Null,
                    Value::Null,
                ],
            ],
        );
        let right = result_set(
            &["ryseSellerId", "email", "createdAt", "phone"],
            vec![
                vec![
                    Value::Float(1.0),
                    Value::Text("a@x.com".into()),
                    Value::Float(1_792_800_000_000.0),
                    Value::Missing,
                ],
                vec![
                    Value::Float(2.0),
                    Value::Text("B@x.com".into()),
                    Value::Missing,
                    Value::Missing,
                ],
                vec![
                    Value::Float(4.0),
                    Value::Text("d@x.com".into()),
                    Value::Missing,
                    Value::Missing,
                ],
            ],
        );
        let left_key = ["id".to_string()];
        let right_key = ["ryseSellerId".to_string()];
        let outcome = compare(
            CompareSide {
                rows: &left,
                key: &left_key,
            },
            CompareSide {
                rows: &right,
                key: &right_key,
            },
            &CompareOptions {
                fields: vec![
                    ("email".into(), "email".into()),
                    ("created".into(), "createdAt".into()),
                    ("phone".into(), "phone".into()),
                ],
            },
        )
        .unwrap();

        assert_eq!(outcome.left_only(), 1);
        assert_eq!(outcome.right_only(), 1);
        assert_eq!(outcome.changed(), 1);
        let CompareRow::Matched { diffs, .. } = &outcome.rows[0] else {
            panic!("row 1 should match");
        };
        assert!(
            diffs.iter().all(|diff| diff.representation_only),
            "{diffs:?}"
        );
        let CompareRow::Matched { diffs, .. } = &outcome.rows[1] else {
            panic!("row 2 should match");
        };
        assert_eq!(
            diffs
                .iter()
                .filter(|diff| !diff.representation_only)
                .count(),
            1
        );
    }

    #[test]
    fn test_large_integer_ids_stay_distinct() {
        let left = result_set(&["id"], vec![vec![Value::Int(9_007_199_254_740_993)]]);
        let right = result_set(&["id"], vec![vec![Value::Int(9_007_199_254_740_992)]]);
        assert_eq!(
            equivalence(&left.rows[0][0], &right.rows[0][0]),
            Equivalence::Different
        );
    }

    #[test]
    fn test_duplicate_keys_are_counted_not_dropped_silently() {
        let left = result_set(&["id"], vec![vec![Value::Int(1)], vec![Value::Int(1)]]);
        let right = result_set(&["id"], vec![vec![Value::Int(1)], vec![Value::Int(1)]]);
        let key = ["id".to_string()];
        let outcome = compare(
            CompareSide {
                rows: &left,
                key: &key,
            },
            CompareSide {
                rows: &right,
                key: &key,
            },
            &CompareOptions::default(),
        )
        .unwrap();
        assert_eq!(outcome.duplicate_left_keys, 1);
        assert_eq!(outcome.duplicate_right_keys, 1);
        assert_eq!(outcome.rows.len(), 1);
        assert_eq!(outcome.right_only(), 0);
    }
}
