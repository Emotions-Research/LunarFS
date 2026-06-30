use crate::cas::Hash;
use std::io;

#[derive(Debug)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub hash: Hash,
}

pub const MODE_DIR: u32 = 0o040000;
pub const MODE_FILE: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;

// Each entry: mode(u32 LE) | name_len(u32 LE) | name bytes | 32-byte hash.
// Entries are sorted by name byte order before serialization for determinism.
pub fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| entries[i].name.as_bytes());

    let mut out = Vec::new();
    for &i in &order {
        let e = &entries[i];
        let name_bytes = e.name.as_bytes();
        out.extend_from_slice(&e.mode.to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&e.hash);
    }
    out
}

pub fn deserialize_tree(data: &[u8]) -> io::Result<Vec<TreeEntry>> {
    const MAX_ENTRIES: usize = 65_536;
    let mut entries = Vec::new();
    let mut pos = 0usize;

    while pos < data.len() {
        if entries.len() >= MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree has too many entries (max 65536)",
            ));
        }
        if pos + 8 > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated tree entry: header too short",
            ));
        }

        let mode = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let name_len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;

        if pos + name_len + 32 > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated tree entry: name or hash too short",
            ));
        }

        let name = String::from_utf8(data[pos..pos + name_len].to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        pos += name_len;

        let hash: Hash = data[pos..pos + 32].try_into().unwrap();
        pos += 32;

        entries.push(TreeEntry { mode, name, hash });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::hash_bytes;

    fn make_entry(name: &str, mode: u32) -> TreeEntry {
        let h = hash_bytes(name.as_bytes());
        TreeEntry {
            mode,
            name: name.to_string(),
            hash: h,
        }
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let entries = vec![
            make_entry("main.rs", MODE_FILE),
            make_entry("lib.rs", MODE_FILE),
            make_entry("src", MODE_DIR),
        ];
        let bytes = serialize_tree(&entries);
        let back = deserialize_tree(&bytes).expect("deserialize must succeed");
        assert_eq!(back.len(), 3, "roundtrip must preserve entry count");
        // After sort: lib.rs, main.rs, src
        assert_eq!(back[0].name, "lib.rs");
        assert_eq!(back[1].name, "main.rs");
        assert_eq!(back[2].name, "src");
        assert_eq!(back[0].mode, MODE_FILE);
        assert_eq!(back[2].mode, MODE_DIR);
    }

    #[test]
    fn serialize_determinism() {
        let a = vec![make_entry("b", MODE_FILE), make_entry("a", MODE_FILE)];
        let b = vec![make_entry("a", MODE_FILE), make_entry("b", MODE_FILE)];
        assert_eq!(
            serialize_tree(&a),
            serialize_tree(&b),
            "serialize must produce identical bytes regardless of input order"
        );
    }

    #[test]
    fn deserialize_empty() {
        let entries = deserialize_tree(&[]).expect("empty bytes must deserialize to empty vec");
        assert!(entries.is_empty());
    }

    #[test]
    fn deserialize_truncated_header() {
        let err = deserialize_tree(&[0u8; 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn deserialize_truncated_body() {
        // Write a valid header (mode=0, name_len=5) then only 2 name bytes
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(b"ab");
        let err = deserialize_tree(&data).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn serialize_single_entry_byte_stable() {
        let h = [0x42u8; 32];
        let entry = TreeEntry {
            mode: MODE_FILE,
            name: "x".to_string(),
            hash: h,
        };
        let bytes = serialize_tree(&[entry]);
        // mode(4) + name_len(4) + "x"(1) + hash(32) = 41 bytes
        assert_eq!(bytes.len(), 41);
        assert_eq!(&bytes[..4], &MODE_FILE.to_le_bytes());
        assert_eq!(&bytes[4..8], &1u32.to_le_bytes());
        assert_eq!(bytes[8], b'x');
        assert_eq!(&bytes[9..41], &[0x42u8; 32]);
    }
}
