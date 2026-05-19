use crate::document::Value;

pub fn serialize_key(value: &Value) -> Vec<u8> {
    match value {
        Value::Null => vec![0x00],

        Value::Bool(false) => vec![0x01],
        Value::Bool(true) => vec![0x02],

        Value::Int(n) => {
            let mut out = Vec::with_capacity(9);
            out.push(0x03);
            let sortable = (*n as u64) ^ (1u64 << 63);
            out.extend_from_slice(&sortable.to_be_bytes());
            out
        }

        Value::Float(n) => {
            assert!(!n.is_nan(), "NaN cannot be used as an index key");

            let bits = n.to_bits();
            let sortable = if bits & (1u64 << 63) != 0 {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };

            let mut out = Vec::with_capacity(9);
            out.push(0x04);
            out.extend_from_slice(&sortable.to_be_bytes());
            out
        }

        Value::String(s) => {
            let mut out = Vec::with_capacity(1 + s.len());
            out.push(0x05);
            out.extend_from_slice(s.as_bytes());
            out
        }

        Value::Array(_) => panic!("array values cannot be used as index keys yet"),
        Value::Document(_) => panic!("document values cannot be used as index keys yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_ordering() {
        let values = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(0),
            Value::Float(0.0),
            Value::String("a".to_string()),
        ];

        let mut keys: Vec<Vec<u8>> = values.iter().map(serialize_key).collect();
        let sorted = keys.clone();
        keys.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn test_int_ordering_handles_negative_values() {
        let values = vec![Value::Int(-10), Value::Int(-1), Value::Int(0), Value::Int(1), Value::Int(10)];
        let mut keys: Vec<Vec<u8>> = values.iter().map(serialize_key).collect();
        let sorted = keys.clone();
        keys.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn test_float_ordering_handles_negative_values() {
        let values = vec![
            Value::Float(-10.5),
            Value::Float(-1.0),
            Value::Float(0.0),
            Value::Float(1.0),
            Value::Float(10.5),
        ];
        let mut keys: Vec<Vec<u8>> = values.iter().map(serialize_key).collect();
        let sorted = keys.clone();
        keys.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    #[should_panic(expected = "NaN cannot be used as an index key")]
    fn test_nan_is_rejected() {
        serialize_key(&Value::Float(f64::NAN));
    }
}
