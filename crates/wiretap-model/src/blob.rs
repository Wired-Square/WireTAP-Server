//! A catalogue's name on the wire: the SHA-1 Git gives the same bytes as a
//! blob, so `git hash-object` names a catalogue file exactly as the gateway
//! and the daemon do.

use sha1::{Digest, Sha1};

pub fn blob_sha1(content: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", content.len()));
    hasher.update(content);
    hasher.finalize().into()
}

/// Lowercase hex, as Git prints it.
pub fn blob_sha1_hex(sha: &[u8; 20]) -> String {
    sha.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_is_named_as_git_hash_object_names_it() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            blob_sha1_hex(&blob_sha1(b"hello\n")),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        // `printf '' | git hash-object --stdin`
        assert_eq!(
            blob_sha1_hex(&blob_sha1(b"")),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn line_endings_are_hashed_as_they_are() {
        assert_ne!(blob_sha1(b"a\r\n"), blob_sha1(b"a\n"));
    }
}
