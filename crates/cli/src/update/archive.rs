//! Verifying a downloaded release archive and getting the binary out of it.

use std::io::Read as _;

use anyhow::Context as _;
use sha2::{Digest as _, Sha256};

/// What the binary is called inside the archive for *this* target — the only
/// archive self-update ever downloads.
#[cfg(windows)]
const BINARY_NAME: &str = "mcpgw.exe";
#[cfg(not(windows))]
const BINARY_NAME: &str = "mcpgw";

/// Lowercase hex SHA-256 of `bytes`, in the spelling `SHA256SUMS` uses.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex: String, byte: &u8| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The recorded hash for `name` in a `sha256sum`-format file.
///
/// The name field may carry a leading `*` (the binary-mode marker some
/// producers write); everything else on the line is the hash.
#[must_use]
pub fn expected_hash<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let file = fields.next()?.trim_start_matches('*');
        (file == name && fields.next().is_none()).then_some(hash)
    })
}

/// Checks a downloaded asset against the release's `SHA256SUMS`.
///
/// # Errors
///
/// When the file is not listed at all (a truncated or wrong sums file) or
/// when the hashes differ.
pub fn verify(sums: &str, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let expected = expected_hash(sums, name)
        .with_context(|| format!("{name} is not listed in the release's SHA256SUMS"))?;
    let actual = sha256_hex(bytes);
    anyhow::ensure!(
        expected.eq_ignore_ascii_case(&actual),
        "checksum mismatch for {name}: SHA256SUMS says {expected}, the download hashes to {actual}"
    );
    Ok(())
}

/// Pulls the mcpgw executable out of a release archive.
///
/// The entry is matched on its file name rather than its full path: the
/// archives wrap the binary in a `mcpgw-<version>-<target>/` directory whose
/// name this function would otherwise have to reconstruct.
///
/// # Errors
///
/// When the archive will not open or contains no mcpgw executable.
pub fn extract_binary(asset: &str, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    if std::path::Path::new(asset)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        return from_zip(bytes);
    }
    from_tar_gz(bytes)
}

fn from_tar_gz(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().context("cannot read the archive")? {
        let mut entry = entry.context("cannot read the archive")?;
        let path = entry.path().context("cannot read an archive entry name")?;
        if path.file_name().is_some_and(|name| name == BINARY_NAME) {
            let mut out = Vec::new();
            entry
                .read_to_end(&mut out)
                .context("cannot read the binary out of the archive")?;
            return Ok(out);
        }
    }
    anyhow::bail!("the archive contains no {BINARY_NAME}")
}

#[cfg(windows)]
fn from_zip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("cannot read the archive")?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("cannot read an archive entry")?;
        let is_binary = entry
            .enclosed_name()
            .is_some_and(|path| path.file_name().is_some_and(|name| name == BINARY_NAME));
        if is_binary {
            let mut out = Vec::new();
            entry
                .read_to_end(&mut out)
                .context("cannot read the binary out of the archive")?;
            return Ok(out);
        }
    }
    anyhow::bail!("the archive contains no {BINARY_NAME}")
}

#[cfg(not(windows))]
fn from_zip(_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    // Only the Windows release is packaged as a zip, so no other build has a
    // zip reader compiled in — and none can be handed a zip asset either,
    // since the asset name is derived from this binary's own target.
    anyhow::bail!("zip archives are only built for Windows targets")
}

#[cfg(test)]
mod tests {
    use super::*;

    // `sha256sum` of the empty input, i.e. a value independent of this
    // implementation.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    const SUMS: &str = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  mcpgw-0.2.0-x86_64-apple-darwin.tar.gz
0000000000000000000000000000000000000000000000000000000000000000  mcpgw-installer.sh
";

    #[test]
    fn hashing_matches_the_reference_digest() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_hash_is_looked_up_by_exact_file_name() {
        assert_eq!(
            expected_hash(SUMS, "mcpgw-0.2.0-x86_64-apple-darwin.tar.gz"),
            Some(EMPTY_SHA256)
        );
        assert_eq!(
            expected_hash(SUMS, "mcpgw-0.2.0-aarch64-apple-darwin.tar.gz"),
            None
        );
        // A prefix of a listed name must not match it.
        assert_eq!(expected_hash(SUMS, "mcpgw-0.2.0"), None);
    }

    #[test]
    fn the_binary_mode_star_is_not_part_of_the_name() {
        let sums = format!("{EMPTY_SHA256} *mcpgw-0.2.0-x86_64-pc-windows-msvc.zip\n");
        assert_eq!(
            expected_hash(&sums, "mcpgw-0.2.0-x86_64-pc-windows-msvc.zip"),
            Some(EMPTY_SHA256)
        );
    }

    #[test]
    fn verification_accepts_the_listed_bytes() {
        verify(SUMS, "mcpgw-0.2.0-x86_64-apple-darwin.tar.gz", b"").unwrap();
    }

    #[test]
    fn verification_rejects_tampered_bytes() {
        let err = verify(SUMS, "mcpgw-0.2.0-x86_64-apple-darwin.tar.gz", b"tampered")
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
    }

    #[test]
    fn verification_rejects_an_unlisted_file() {
        let err = verify(SUMS, "mcpgw-9.9.9-x86_64-apple-darwin.tar.gz", b"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not listed"), "{err}");
    }

    #[test]
    fn an_uppercase_sums_file_still_verifies() {
        let sums = format!("{} mcpgw.tar.gz\n", EMPTY_SHA256.to_uppercase());
        verify(&sums, "mcpgw.tar.gz", b"").unwrap();
    }

    /// Builds the archive layout release.yml produces: the binary inside a
    /// `mcpgw-<version>-<target>/` directory, next to the shipped docs.
    #[cfg(not(windows))]
    fn tarball(binary: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut append = |name: &str, bytes: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, bytes).unwrap();
        };
        append("mcpgw-0.2.0-x86_64-apple-darwin/README.md", b"# mcpgw");
        append("mcpgw-0.2.0-x86_64-apple-darwin/mcpgw", binary);
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(not(windows))]
    #[test]
    fn the_binary_comes_out_of_the_versioned_directory() {
        let archive = tarball(b"ELF-ish payload");
        let found = extract_binary("mcpgw-0.2.0-x86_64-apple-darwin.tar.gz", &archive).unwrap();
        assert_eq!(found, b"ELF-ish payload");
    }

    #[cfg(not(windows))]
    #[test]
    fn an_archive_without_the_binary_is_an_error() {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let bytes = b"# mcpgw";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "mcpgw-0.2.0/README.md", &bytes[..])
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let err = extract_binary("mcpgw-0.2.0.tar.gz", &archive)
            .unwrap_err()
            .to_string();
        assert!(err.contains("contains no"), "{err}");
    }

    #[test]
    fn a_corrupt_archive_is_an_error_not_a_panic() {
        assert!(extract_binary("mcpgw-0.2.0.tar.gz", b"not a gzip stream").is_err());
    }
}
