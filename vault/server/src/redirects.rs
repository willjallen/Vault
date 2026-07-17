use reqwest::Url;

const REDIRECT_BASE: &str = "https://vault.invalid/";
const MAX_PERCENT_DECODE_PASSES: usize = 8;

#[must_use]
pub fn safe_redirect(value: Option<&str>) -> String {
    value
        .filter(|candidate| is_safe_origin_relative_redirect(candidate))
        .unwrap_or("/")
        .to_string()
}

fn is_safe_origin_relative_redirect(value: &str) -> bool {
    if !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || !has_safe_percent_encodings(value)
    {
        return false;
    }
    let Ok(base) = Url::parse(REDIRECT_BASE) else {
        return false;
    };
    let Ok(resolved) = base.join(value) else {
        return false;
    };
    resolved.origin() == base.origin()
        && resolved.username().is_empty()
        && resolved.password().is_none()
}

fn has_safe_percent_encodings(value: &str) -> bool {
    let mut current = value.as_bytes().to_vec();
    let mut require_valid_triplets = true;
    for pass in 0..MAX_PERCENT_DECODE_PASSES {
        let mut decoded = Vec::with_capacity(current.len());
        let mut decoded_triplet = false;
        let mut index = 0;
        while index < current.len() {
            if current[index] != b'%' {
                decoded.push(current[index]);
                index += 1;
                continue;
            }
            let triplet = current
                .get(index + 1..index + 3)
                .and_then(|digits| percent_byte(digits[0], digits[1]));
            let Some(byte) = triplet else {
                if require_valid_triplets {
                    return false;
                }
                decoded.push(current[index]);
                index += 1;
                continue;
            };
            if is_dangerous_decoded_byte(byte) {
                return false;
            }
            decoded.push(byte);
            decoded_triplet = true;
            index += 3;
        }
        let Ok(decoded_text) = std::str::from_utf8(&decoded) else {
            return false;
        };
        if decoded_text.chars().any(char::is_control) {
            return false;
        }
        if !decoded_triplet {
            return true;
        }
        if pass + 1 == MAX_PERCENT_DECODE_PASSES && contains_percent_triplet(&decoded) {
            return false;
        }
        current = decoded;
        require_valid_triplets = false;
    }
    true
}

fn contains_percent_triplet(value: &[u8]) -> bool {
    value
        .windows(3)
        .any(|window| window[0] == b'%' && percent_byte(window[1], window[2]).is_some())
}

fn percent_byte(high: u8, low: u8) -> Option<u8> {
    Some((hex_nibble(high)? << 4) | hex_nibble(low)?)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn is_dangerous_decoded_byte(value: u8) -> bool {
    value < b' ' || value == b'\x7f' || matches!(value, b'/' | b'\\')
}
