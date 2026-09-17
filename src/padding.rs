use md5::{Digest, Md5};
use rand::RngExt;

pub const CHECK_MARK: i32 = -1;
pub const DEFAULT_SCHEME: &[u8] = b"stop=8\n0=30-30\n1=100-400\n2=400-500,c,500-1000,c,500-1000,c,500-1000,c,500-1000\n3=9-9,500-1000\n4=500-1000\n5=500-1000\n6=500-1000\n7=500-1000";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaddingFactory {
    raw_scheme: Vec<u8>,
    pub stop: u32,
    pub md5: String,
    scheme: crate::string_map::StringMap,
}

impl PaddingFactory {
    pub fn new(raw_scheme: &[u8]) -> Option<Self> {
        let scheme = crate::string_map::from_bytes(raw_scheme);
        let stop = scheme.get("stop")?.parse().ok()?;
        let mut digest = Md5::new();
        digest.update(raw_scheme);
        Some(Self {
            raw_scheme: raw_scheme.to_vec(),
            stop,
            md5: digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect(),
            scheme,
        })
    }

    pub fn raw_scheme(&self) -> &[u8] {
        &self.raw_scheme
    }

    pub fn generate_record_payload_sizes(&self, packet: u32) -> Vec<i32> {
        let Some(value) = self.scheme.get(&packet.to_string()) else {
            return Vec::new();
        };
        value.split(',').filter_map(parse_range).collect()
    }
}

fn parse_range(value: &str) -> Option<i32> {
    if value == "c" {
        return Some(CHECK_MARK);
    }
    let (minimum, maximum) = value.split_once('-')?;
    let mut minimum: i64 = minimum.parse().ok()?;
    let mut maximum: i64 = maximum.parse().ok()?;
    if minimum > maximum {
        std::mem::swap(&mut minimum, &mut maximum);
    }
    if minimum <= 0 || maximum <= 0 {
        return None;
    }
    if minimum == maximum {
        return Some(minimum as i32);
    }
    Some(rand::rng().random_range(minimum..maximum) as i32)
}

impl std::fmt::Display for PaddingFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.md5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_default_scheme_metadata_and_ranges() {
        let factory = PaddingFactory::new(DEFAULT_SCHEME).unwrap();
        assert_eq!(factory.stop, 8);
        assert_eq!(factory.md5, "75cff2ad89aadf5e257059ee571ebe11");
        assert_eq!(factory.generate_record_payload_sizes(0), vec![30]);
        assert_eq!(factory.generate_record_payload_sizes(2).len(), 9);
        assert!(factory.generate_record_payload_sizes(2).contains(&CHECK_MARK));
    }

    #[test]
    fn rejects_scheme_without_stop() {
        assert!(PaddingFactory::new(b"0=1-2").is_none());
    }
}
