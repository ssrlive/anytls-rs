use std::collections::HashMap;

pub type StringMap = HashMap<String, String>;

pub fn from_bytes(bytes: &[u8]) -> StringMap {
    let mut map = StringMap::new();
    let text = String::from_utf8_lossy(bytes);
    for line in text.split('\n') {
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_owned(), value.to_owned());
        }
    }
    map
}

pub fn to_bytes(map: &StringMap) -> Vec<u8> {
    map.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_first_equals_like_go_splitn() {
        let map = from_bytes(b"v=2\nclient=anytls/0.0.13\nvalue=a=b\nignored");
        assert_eq!(map.get("v").unwrap(), "2");
        assert_eq!(map.get("value").unwrap(), "a=b");
        assert!(!map.contains_key("ignored"));
    }
}
