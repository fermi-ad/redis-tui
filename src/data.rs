use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataType {
    Int8,
    Int16,
    Int32,
    UInt8,
    UInt16,
    UInt32,
    Float32,
    Float64,
    String,
    Blob,
}

impl DataType {
    #[allow(dead_code)]
    pub fn byte_size(&self) -> Option<usize> {
        match self {
            DataType::Int8 | DataType::UInt8 => Some(1),
            DataType::Int16 | DataType::UInt16 => Some(2),
            DataType::Int32 | DataType::UInt32 | DataType::Float32 => Some(4),
            DataType::Float64 => Some(8),
            DataType::String | DataType::Blob => None,
        }
    }

    pub fn all() -> &'static [DataType] {
        &[
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::Float32,
            DataType::Float64,
            DataType::String,
            DataType::Blob,
        ]
    }

    pub fn next(&self) -> DataType {
        let all = Self::all();
        let idx = all.iter().position(|t| t == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    pub fn prev(&self) -> DataType {
        let all = Self::all();
        let idx = all.iter().position(|t| t == self).unwrap_or(0);
        if idx == 0 {
            all[all.len() - 1]
        } else {
            all[idx - 1]
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Int8 => write!(f, "int8"),
            DataType::Int16 => write!(f, "int16"),
            DataType::Int32 => write!(f, "int32"),
            DataType::UInt8 => write!(f, "uint8"),
            DataType::UInt16 => write!(f, "uint16"),
            DataType::UInt32 => write!(f, "uint32"),
            DataType::Float32 => write!(f, "float32"),
            DataType::Float64 => write!(f, "float64"),
            DataType::String => write!(f, "string"),
            DataType::Blob => write!(f, "blob/hex"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Endianness {
    Little,
    Big,
}

impl Endianness {
    pub fn toggle(&self) -> Endianness {
        match self {
            Endianness::Little => Endianness::Big,
            Endianness::Big => Endianness::Little,
        }
    }
}

impl fmt::Display for Endianness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Endianness::Little => write!(f, "LE"),
            Endianness::Big => write!(f, "BE"),
        }
    }
}

/// Decode raw bytes into a vector of f64 values for plotting
pub fn decode_blob(bytes: &[u8], data_type: DataType, endianness: Endianness) -> Vec<f64> {
    match data_type {
        DataType::Int8 => bytes.iter().map(|&b| (b as i8) as f64).collect(),
        DataType::UInt8 => bytes.iter().map(|&b| b as f64).collect(),
        DataType::Int16 => decode_chunks(bytes, |arr: [u8; 2]| match endianness {
            Endianness::Little => i16::from_le_bytes(arr) as f64,
            Endianness::Big => i16::from_be_bytes(arr) as f64,
        }),
        DataType::UInt16 => decode_chunks(bytes, |arr: [u8; 2]| match endianness {
            Endianness::Little => u16::from_le_bytes(arr) as f64,
            Endianness::Big => u16::from_be_bytes(arr) as f64,
        }),
        DataType::Int32 => decode_chunks(bytes, |arr: [u8; 4]| match endianness {
            Endianness::Little => i32::from_le_bytes(arr) as f64,
            Endianness::Big => i32::from_be_bytes(arr) as f64,
        }),
        DataType::UInt32 => decode_chunks(bytes, |arr: [u8; 4]| match endianness {
            Endianness::Little => u32::from_le_bytes(arr) as f64,
            Endianness::Big => u32::from_be_bytes(arr) as f64,
        }),
        DataType::Float32 => decode_chunks(bytes, |arr: [u8; 4]| match endianness {
            Endianness::Little => f32::from_le_bytes(arr) as f64,
            Endianness::Big => f32::from_be_bytes(arr) as f64,
        }),
        DataType::Float64 => decode_chunks(bytes, |arr: [u8; 8]| match endianness {
            Endianness::Little => f64::from_le_bytes(arr),
            Endianness::Big => f64::from_be_bytes(arr),
        }),
        DataType::String | DataType::Blob => {
            // For string/blob, just plot byte values
            bytes.iter().map(|&b| b as f64).collect()
        }
    }
}

fn decode_chunks<const N: usize>(bytes: &[u8], f: impl Fn([u8; N]) -> f64) -> Vec<f64> {
    bytes
        .chunks_exact(N)
        .map(|chunk| {
            // chunks_exact(N) guarantees chunk.len() == N, so this is infallible
            let mut arr = [0u8; N];
            arr.copy_from_slice(chunk);
            f(arr)
        })
        .collect()
}

/// Format raw bytes as a human-readable string according to data type
#[allow(dead_code)]
pub fn format_blob(bytes: &[u8], data_type: DataType, endianness: Endianness) -> String {
    match data_type {
        DataType::Blob => format_hex(bytes),
        DataType::String => String::from_utf8_lossy(bytes).to_string(),
        _ => {
            let values = decode_blob(bytes, data_type, endianness);
            if values.is_empty() {
                return "(no complete values)".to_string();
            }
            let formatted: Vec<String> = values
                .iter()
                .map(|v| {
                    match data_type {
                        DataType::Float32 | DataType::Float64 => format!("{:.6}", v),
                        _ => format!("{}", *v as i64),
                    }
                })
                .collect();
            formatted.join(", ")
        }
    }
}

/// Format bytes as a hex dump with ASCII sidebar
pub fn format_hex(bytes: &[u8]) -> String {
    let mut result = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        // Offset
        result.push_str(&format!("{:08x}  ", i * 16));
        // Hex bytes
        for (j, byte) in chunk.iter().enumerate() {
            result.push_str(&format!("{:02x} ", byte));
            if j == 7 {
                result.push(' ');
            }
        }
        // Padding for incomplete lines
        let padding = 16 - chunk.len();
        for j in 0..padding {
            result.push_str("   ");
            if chunk.len() + j == 7 {
                result.push(' ');
            }
        }
        // ASCII
        result.push_str(" |");
        for byte in chunk {
            if byte.is_ascii_graphic() || *byte == b' ' {
                result.push(*byte as char);
            } else {
                result.push('.');
            }
        }
        result.push_str("|\n");
    }
    result
}

/// Encode a string of comma/space-separated numeric values into binary bytes.
/// Supports ints and floats depending on the target DataType.
pub fn encode_values(input: &str, data_type: DataType, endianness: Endianness) -> Result<Vec<u8>, String> {
    let tokens: Vec<&str> = input
        .split(|c: char| c == ',' || c == ' ')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if tokens.is_empty() {
        return Err("No values to encode".to_string());
    }

    let mut bytes = Vec::new();

    for token in &tokens {
        match data_type {
            DataType::Int8 => {
                let v: i8 = token.parse().map_err(|_| format!("'{}' is not a valid i8", token))?;
                bytes.push(v as u8);
            }
            DataType::UInt8 => {
                let v: u8 = token.parse().map_err(|_| format!("'{}' is not a valid u8", token))?;
                bytes.push(v);
            }
            DataType::Int16 => {
                let v: i16 = token.parse().map_err(|_| format!("'{}' is not a valid i16", token))?;
                match endianness {
                    Endianness::Little => bytes.extend_from_slice(&v.to_le_bytes()),
                    Endianness::Big => bytes.extend_from_slice(&v.to_be_bytes()),
                }
            }
            DataType::UInt16 => {
                let v: u16 = token.parse().map_err(|_| format!("'{}' is not a valid u16", token))?;
                match endianness {
                    Endianness::Little => bytes.extend_from_slice(&v.to_le_bytes()),
                    Endianness::Big => bytes.extend_from_slice(&v.to_be_bytes()),
                }
            }
            DataType::Int32 => {
                let v: i32 = token.parse().map_err(|_| format!("'{}' is not a valid i32", token))?;
                match endianness {
                    Endianness::Little => bytes.extend_from_slice(&v.to_le_bytes()),
                    Endianness::Big => bytes.extend_from_slice(&v.to_be_bytes()),
                }
            }
            DataType::UInt32 => {
                let v: u32 = token.parse().map_err(|_| format!("'{}' is not a valid u32", token))?;
                match endianness {
                    Endianness::Little => bytes.extend_from_slice(&v.to_le_bytes()),
                    Endianness::Big => bytes.extend_from_slice(&v.to_be_bytes()),
                }
            }
            DataType::Float32 => {
                let v: f32 = token.parse().map_err(|_| format!("'{}' is not a valid f32", token))?;
                match endianness {
                    Endianness::Little => bytes.extend_from_slice(&v.to_le_bytes()),
                    Endianness::Big => bytes.extend_from_slice(&v.to_be_bytes()),
                }
            }
            DataType::Float64 => {
                let v: f64 = token.parse().map_err(|_| format!("'{}' is not a valid f64", token))?;
                match endianness {
                    Endianness::Little => bytes.extend_from_slice(&v.to_le_bytes()),
                    Endianness::Big => bytes.extend_from_slice(&v.to_be_bytes()),
                }
            }
            DataType::String | DataType::Blob => {
                return Err("Binary encode not supported for String/Blob types".to_string());
            }
        }
    }

    Ok(bytes)
}

/// Check if bytes look like they contain binary (non-UTF8 or control chars)
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- decode_blob: normal decoding ----

    #[test]
    fn decode_int16_le() {
        // 0x0100 = 256 as little-endian i16
        let bytes = [0x00, 0x01];
        let result = decode_blob(&bytes, DataType::Int16, Endianness::Little);
        assert_eq!(result, vec![256.0]);
    }

    #[test]
    fn decode_int16_be() {
        let bytes = [0x01, 0x00];
        let result = decode_blob(&bytes, DataType::Int16, Endianness::Big);
        assert_eq!(result, vec![256.0]);
    }

    #[test]
    fn decode_int16_negative() {
        // -1 as little-endian i16 = [0xFF, 0xFF]
        let bytes = [0xFF, 0xFF];
        let result = decode_blob(&bytes, DataType::Int16, Endianness::Little);
        assert_eq!(result, vec![-1.0]);
    }

    #[test]
    fn decode_uint32_le() {
        let bytes = 1000u32.to_le_bytes();
        let result = decode_blob(&bytes, DataType::UInt32, Endianness::Little);
        assert_eq!(result, vec![1000.0]);
    }

    #[test]
    fn decode_float32_le() {
        let bytes = std::f32::consts::PI.to_le_bytes();
        let result = decode_blob(&bytes, DataType::Float32, Endianness::Little);
        assert_eq!(result.len(), 1);
        assert!((result[0] - std::f64::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn decode_float64_be() {
        let bytes = 42.5f64.to_be_bytes();
        let result = decode_blob(&bytes, DataType::Float64, Endianness::Big);
        assert_eq!(result, vec![42.5]);
    }

    #[test]
    fn decode_int8() {
        let bytes = [0x80, 0x7F]; // -128, 127 as i8
        let result = decode_blob(&bytes, DataType::Int8, Endianness::Little);
        assert_eq!(result, vec![-128.0, 127.0]);
    }

    #[test]
    fn decode_uint8() {
        let bytes = [0, 128, 255];
        let result = decode_blob(&bytes, DataType::UInt8, Endianness::Little);
        assert_eq!(result, vec![0.0, 128.0, 255.0]);
    }

    #[test]
    fn decode_multiple_values() {
        // Two i16 values: 1, 2
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i16.to_le_bytes());
        bytes.extend_from_slice(&2i16.to_le_bytes());
        let result = decode_blob(&bytes, DataType::Int16, Endianness::Little);
        assert_eq!(result, vec![1.0, 2.0]);
    }

    // ---- decode_blob: edge cases (no panics) ----

    #[test]
    fn decode_empty_bytes() {
        let result = decode_blob(&[], DataType::Int16, Endianness::Little);
        assert!(result.is_empty());
    }

    #[test]
    fn decode_odd_size_bytes_discards_remainder() {
        // 3 bytes for i16 (chunk_size=2): should decode 1 value, discard trailing byte
        let bytes = [0x01, 0x00, 0xFF];
        let result = decode_blob(&bytes, DataType::Int16, Endianness::Little);
        assert_eq!(result, vec![1.0]);
    }

    #[test]
    fn decode_undersized_bytes_returns_empty() {
        // 1 byte for i32 (chunk_size=4): no complete chunks
        let result = decode_blob(&[0xFF], DataType::Int32, Endianness::Little);
        assert!(result.is_empty());
    }

    #[test]
    fn decode_undersized_float64_returns_empty() {
        // 7 bytes for f64 (chunk_size=8): no complete chunks
        let result = decode_blob(&[0; 7], DataType::Float64, Endianness::Little);
        assert!(result.is_empty());
    }

    #[test]
    fn decode_blob_string_type() {
        let bytes = b"hello";
        let result = decode_blob(bytes, DataType::String, Endianness::Little);
        // String type plots raw byte values
        assert_eq!(result, vec![104.0, 101.0, 108.0, 108.0, 111.0]);
    }

    // ---- decode_chunks: const generic safety ----

    #[test]
    fn decode_chunks_exact_fit() {
        let result: Vec<f64> = decode_chunks(&[1, 0, 2, 0], |arr: [u8; 2]| {
            u16::from_le_bytes(arr) as f64
        });
        assert_eq!(result, vec![1.0, 2.0]);
    }

    #[test]
    fn decode_chunks_with_remainder() {
        // 5 bytes, chunk_size=2: 2 complete chunks, 1 byte remainder discarded
        let result: Vec<f64> = decode_chunks(&[1, 0, 2, 0, 0xFF], |arr: [u8; 2]| {
            u16::from_le_bytes(arr) as f64
        });
        assert_eq!(result, vec![1.0, 2.0]);
    }

    #[test]
    fn decode_chunks_empty() {
        let result: Vec<f64> = decode_chunks(&[], |arr: [u8; 4]| {
            f32::from_le_bytes(arr) as f64
        });
        assert!(result.is_empty());
    }

    // ---- encode/decode round-trip ----

    #[test]
    fn roundtrip_int16_le() {
        let input = "100, -200, 32767";
        let bytes = encode_values(input, DataType::Int16, Endianness::Little).unwrap();
        let decoded = decode_blob(&bytes, DataType::Int16, Endianness::Little);
        assert_eq!(decoded, vec![100.0, -200.0, 32767.0]);
    }

    #[test]
    fn roundtrip_float32_be() {
        let input = "1.5, -3.25";
        let bytes = encode_values(input, DataType::Float32, Endianness::Big).unwrap();
        let decoded = decode_blob(&bytes, DataType::Float32, Endianness::Big);
        assert_eq!(decoded.len(), 2);
        assert!((decoded[0] - 1.5).abs() < 1e-6);
        assert!((decoded[1] - (-3.25)).abs() < 1e-6);
    }
}
