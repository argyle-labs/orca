#![allow(clippy::disallowed_types)]

use serde_json::Value;

pub fn wire_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn push_query(out: &mut Vec<(String, String)>, key: &str, v: &Value) {
    match v {
        Value::Array(items) => {
            for it in items {
                out.push((key.to_string(), wire_string(it)));
            }
        }
        other => out.push((key.to_string(), wire_string(other))),
    }
}

pub fn bool_ish(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub fn number_ish(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_string_renders_scalars_and_json_encodes_compounds() {
        assert_eq!(wire_string(&json!("a b")), "a b");
        assert_eq!(wire_string(&json!(true)), "true");
        assert_eq!(wire_string(&json!(3)), "3");
        assert_eq!(wire_string(&Value::Null), "");
        assert_eq!(wire_string(&json!({"k":1})), r#"{"k":1}"#);
    }

    #[test]
    fn push_query_explodes_arrays() {
        let mut out = Vec::new();
        push_query(&mut out, "k", &json!(["a", 2]));
        push_query(&mut out, "x", &json!(false));
        assert_eq!(
            out,
            vec![
                ("k".into(), "a".into()),
                ("k".into(), "2".into()),
                ("x".into(), "false".into())
            ]
        );
    }

    #[test]
    fn bool_ish_accepts_ints_and_words() {
        assert_eq!(bool_ish(&json!(1)), Some(true));
        assert_eq!(bool_ish(&json!(0)), Some(false));
        assert_eq!(bool_ish(&json!(" Off ")), Some(false));
        assert_eq!(bool_ish(&json!("maybe")), None);
        assert_eq!(bool_ish(&Value::Null), None);
    }

    #[test]
    fn number_ish_accepts_quoted_numerics() {
        assert_eq!(number_ish(&json!("0.00")), Some(0.0));
        assert_eq!(number_ish(&json!(" 42 ")), Some(42.0));
        assert_eq!(number_ish(&json!("x")), None);
        assert_eq!(number_ish(&json!(true)), None);
    }
}
