// Custom Base32 encoding using alphabet: 23456789abcdefghijkmnpqrstuvwxyz
// Excludes visually ambiguous characters: 0, 1, l, o

const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyz";

fn decode_table() -> [u8; 256] {
    let mut table = [0xFFu8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    table
}

pub fn encode(data: &[u8]) -> String {
    let mut output = Vec::with_capacity((data.len() * 8).div_ceil(5));
    let mut buf: u64 = 0;
    let mut bits = 0u32;

    for &byte in data {
        buf = (buf << 8) | byte as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buf >> bits) & 0x1F) as usize;
            output.push(ALPHABET[idx]);
        }
    }
    if bits > 0 {
        let idx = ((buf << (5 - bits)) & 0x1F) as usize;
        output.push(ALPHABET[idx]);
    }

    String::from_utf8(output).expect("alphabet is valid UTF-8")
}

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    let table = decode_table();
    let mut output = Vec::with_capacity(s.len() * 5 / 8);
    let mut buf: u64 = 0;
    let mut bits = 0u32;

    for c in s.bytes() {
        let val = table[c as usize];
        if val == 0xFF {
            return Err(format!(
                "invalid character in connection string: '{}'",
                c as char
            ));
        }
        buf = (buf << 5) | val as u64;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            output.push(((buf >> bits) & 0xFF) as u8);
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alphabet_length() {
        assert_eq!(ALPHABET.len(), 32);
    }

    #[test]
    fn test_alphabet_no_ambiguous_chars() {
        let s = std::str::from_utf8(ALPHABET).unwrap();
        assert!(!s.contains('0'), "alphabet must not contain '0'");
        assert!(!s.contains('1'), "alphabet must not contain '1'");
        assert!(!s.contains('l'), "alphabet must not contain 'l'");
        assert!(!s.contains('o'), "alphabet must not contain 'o'");
    }

    #[test]
    fn test_roundtrip_empty() {
        assert_eq!(decode(&encode(b"")).unwrap(), b"");
    }

    #[test]
    fn test_roundtrip_single_byte() {
        let data = b"\xff";
        assert_eq!(decode(&encode(data)).unwrap(), data);
    }

    #[test]
    fn test_roundtrip_json() {
        let json =
            r#"{"type":"s3","bucket":"my-bucket","prefix":"my-repo","region":"ap-northeast-1"}"#;
        let encoded = encode(json.as_bytes());
        assert!(encoded.chars().all(|c| ALPHABET.contains(&(c as u8))));
        assert_eq!(decode(&encoded).unwrap(), json.as_bytes());
    }

    #[test]
    fn test_roundtrip_all_bytes() {
        let data: Vec<u8> = (0..=255).collect();
        let encoded = encode(&data);
        assert_eq!(decode(&encoded).unwrap(), data);
    }

    #[test]
    fn test_only_alphabet_chars() {
        let data = b"hello world 12345";
        let encoded = encode(data);
        for c in encoded.chars() {
            assert!(ALPHABET.contains(&(c as u8)), "unexpected char: '{}'", c);
        }
    }

    #[test]
    fn test_decode_invalid_char() {
        assert!(decode("invalid!").is_err());
    }

    #[test]
    fn test_decode_zero_rejected() {
        assert!(decode("0").is_err());
    }

    #[test]
    fn test_decode_one_rejected() {
        assert!(decode("1").is_err());
    }
}
