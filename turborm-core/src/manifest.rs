//! Firmware schema-4 manifests: see ../HASH-TREE-SCHEMA4.md.
//! The hash covers every serialized byte, including both headers and final LF.
use anyhow::{anyhow, ensure, Result};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub hash: String,
    pub flags: u32,
    pub name: String,
    pub count: u32,
    /// Leaf byte length, or the sum of an entry's component byte lengths.
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub flags: u32,
    pub rows: Vec<Row>,
}

fn valid_name(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c == ':' || c.is_control())
}

fn flags(value: &str) -> Result<u32> {
    ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Invalid manifest flags"
    );
    let result = u32::from_str_radix(value, 16).map_err(|_| anyhow!("Manifest flags overflow"))?;
    ensure!(
        format!("{result:x}") == value,
        "Noncanonical manifest flags"
    );
    Ok(result)
}

fn decimal(value: &str) -> Result<u64> {
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "Invalid manifest integer"
    );
    let result = value
        .parse::<u64>()
        .map_err(|_| anyhow!("Manifest integer overflow"))?;
    ensure!(
        result <= i64::MAX as u64,
        "Manifest integer exceeds signed 64-bit range"
    );
    ensure!(result.to_string() == value, "Noncanonical manifest integer");
    Ok(result)
}

fn count(value: &str) -> Result<u32> {
    u32::try_from(decimal(value)?).map_err(|_| anyhow!("Manifest child count overflow"))
}

pub fn raw_hash(bytes: &[u8]) -> String {
    crate::job::hash(bytes)
}

impl Manifest {
    /// Strict application parser. Unknown flag bits are retained unchanged.
    /// Call raw_hash on the downloaded bytes before parsing a remote manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("Manifest is not UTF-8"))?;
        ensure!(
            text.ends_with('\n') && !text.contains('\r'),
            "Manifest must use terminated LF lines"
        );
        let mut lines = text[..text.len() - 1].split('\n');
        ensure!(
            lines.next() == Some("4"),
            "Unsupported manifest schema; expected 4"
        );
        let description = lines
            .next()
            .ok_or_else(|| anyhow!("Missing manifest description"))?;
        let fields = description.split(':').collect::<Vec<_>>();
        ensure!(fields.len() == 4, "Invalid manifest description fields");
        let declared_count = count(fields[2])?;
        let declared_size = decimal(fields[3])?;
        let mut manifest = Self {
            name: fields[1].to_owned(),
            flags: flags(fields[0])?,
            rows: Vec::new(),
        };
        for line in lines {
            let fields = line.split(':').collect::<Vec<_>>();
            ensure!(fields.len() == 5, "Invalid manifest row fields");
            manifest.rows.push(Row {
                hash: fields[0].to_owned(),
                flags: flags(fields[1])?,
                name: fields[2].to_owned(),
                count: count(fields[3])?,
                size: decimal(fields[4])?,
            });
        }
        manifest.validate()?;
        ensure!(
            manifest.rows.len() == declared_count as usize,
            "Manifest description count does not match rows"
        );
        ensure!(
            manifest.total_size()? == declared_size,
            "Manifest description size does not match rows"
        );
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        ensure!(valid_name(&self.name), "Invalid manifest name");
        ensure!(
            self.name == "." || uuid::Uuid::parse_str(&self.name).is_ok_and(|u| !u.is_nil()),
            "Manifest description must identify the root or a document"
        );
        ensure!(
            u32::try_from(self.rows.len()).is_ok(),
            "Manifest has too many rows"
        );
        let mut seen = HashSet::with_capacity(self.rows.len());
        for row in &self.rows {
            ensure!(valid_name(&row.name), "Invalid manifest row name");
            ensure!(seen.insert(&row.name), "Duplicate manifest row name");
            ensure!(
                crate::cloud::is_hash(&row.hash),
                "Invalid manifest row hash"
            );
            ensure!(
                row.size <= i64::MAX as u64,
                "Manifest row size exceeds signed 64-bit range"
            );
        }
        self.total_size()?;
        Ok(())
    }

    pub fn total_size(&self) -> Result<u64> {
        self.rows.iter().try_fold(0u64, |sum, row| {
            sum.checked_add(row.size)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| anyhow!("Manifest aggregate size overflow"))
        })
    }

    /// Serializes rows by ascending raw UTF-8 path bytes, as the native vectors do.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut rows = self.rows.iter().collect::<Vec<_>>();
        rows.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let mut text = format!(
            "4\n{:x}:{}:{}:{}\n",
            self.flags,
            self.name,
            rows.len(),
            self.total_size()?
        );
        for row in rows {
            use std::fmt::Write;
            writeln!(
                text,
                "{}:{:x}:{}:{}:{}",
                row.hash, row.flags, row.name, row.count, row.size
            )?;
        }
        Ok(text.into_bytes())
    }

    pub fn hash(&self) -> Result<String> {
        Ok(raw_hash(&self.serialize()?))
    }

    /// Creates only the new document manifest. Job payload validation is separate.
    pub fn from_job(job: &crate::job::Job) -> Result<Self> {
        let name = job.uuid.to_string();
        ensure!(
            !job.uuid.is_nil() && !job.files.is_empty(),
            "Invalid new document identity or components"
        );
        let flat_prefix = format!("{name}.");
        let nested_prefix = format!("{name}/");
        let mut rows = Vec::with_capacity(job.files.len());
        for part in &job.files {
            ensure!(
                part.path.starts_with(&flat_prefix) || part.path.starts_with(&nested_prefix),
                "Component does not belong to the new document"
            );
            rows.push(Row {
                hash: part.hash.clone(),
                flags: 0,
                name: part.path.clone(),
                count: 0,
                size: part.size,
            });
        }
        let manifest = Self {
            name,
            flags: 0,
            rows,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const DOC: &str = "11111111-1111-4111-8111-111111111111";
    fn document() -> Manifest {
        let parts: [(&str, &[u8]); 3] = [
            ("content", b"synthetic content"),
            ("metadata", b"synthetic metadata"),
            ("pdf", b"synthetic PDF bytes"),
        ];
        Manifest {
            name: DOC.into(),
            flags: 0,
            rows: parts
                .into_iter()
                .map(|(ext, data)| Row {
                    hash: raw_hash(data),
                    flags: 0,
                    name: format!("{DOC}.{ext}"),
                    count: 0,
                    size: data.len() as u64,
                })
                .collect(),
        }
    }
    #[test]
    fn exact_schema4_bytes_and_independent_python_digest() {
        let doc = document();
        let expected = format!("4\n0:{DOC}:3:54\n{}:0:{DOC}.content:0:17\n{}:0:{DOC}.metadata:0:18\n{}:0:{DOC}.pdf:0:19\n",
            raw_hash(b"synthetic content"), raw_hash(b"synthetic metadata"), raw_hash(b"synthetic PDF bytes"));
        assert_eq!(doc.serialize().unwrap(), expected.as_bytes());
        assert_eq!(
            doc.hash().unwrap(),
            "7d58f79246df8a4f2e2aff1cc408248924bb091a9bbcc3ffa64c856090dace77"
        );
        assert_eq!(Manifest::parse(expected.as_bytes()).unwrap(), doc);
        let root = Manifest {
            name: ".".into(),
            flags: 0,
            rows: vec![Row {
                hash: doc.hash().unwrap(),
                flags: 0,
                name: DOC.into(),
                count: 3,
                size: 54,
            }],
        };
        assert_eq!(
            root.hash().unwrap(),
            "fc8d10988feb38dce1d584b62358693f56eb3e183c3186d171c7f2b4e521bcdf"
        );
    }
    #[test]
    fn preserves_existing_rows_and_unknown_flags_when_adding() {
        let original = Manifest {
            name: ".".into(),
            flags: 0xa0000000,
            rows: vec![Row {
                hash: "ab".repeat(32),
                flags: 0x80000012,
                name: "22222222-2222-4222-8222-222222222222".into(),
                count: 19,
                size: 713,
            }],
        };
        let mut updated = Manifest::parse(&original.serialize().unwrap()).unwrap();
        let preserved = updated.rows[0].clone();
        let doc = document();
        updated.rows.push(Row {
            hash: doc.hash().unwrap(),
            flags: 0,
            name: DOC.into(),
            count: 3,
            size: 54,
        });
        let parsed = Manifest::parse(&updated.serialize().unwrap()).unwrap();
        assert_eq!(parsed.flags, original.flags);
        assert_eq!(parsed.rows[1], preserved);
        assert_eq!(parsed.total_size().unwrap(), 767);
        assert_eq!(parsed.rows[0].name, DOC);
    }
    #[test]
    fn canonical_order_is_path_order_and_all_bytes_are_hashed() {
        let doc = document();
        let mut reverse = doc.clone();
        reverse.rows.reverse();
        assert_eq!(doc.serialize().unwrap(), reverse.serialize().unwrap());
        let bytes = doc.serialize().unwrap();
        assert_ne!(raw_hash(&bytes), raw_hash(&bytes[..bytes.len() - 1]));
        reverse.flags = 1;
        assert_ne!(doc.hash().unwrap(), reverse.hash().unwrap());
        reverse.flags = 0;
        reverse.rows[0].count = 1;
        assert_ne!(doc.hash().unwrap(), reverse.hash().unwrap());
    }
    #[test]
    fn rejects_duplicate_names_malformed_fields_and_totals() {
        let doc = document();
        let good = String::from_utf8(doc.serialize().unwrap()).unwrap();
        let bad = [
            good.replacen("4\n", "3\n", 1),
            good.replace('\n', "\r\n"),
            good.trim_end().to_owned(),
            good.replacen(":3:54\n", ":4:54\n", 1),
            good.replacen(":3:54\n", ":3:55\n", 1),
            good.replacen("0:", "00:", 1),
            good.replacen(":3:54", ":3:-1", 1),
            format!("{good}\n"),
            good.replacen(":3:54", ":4294967296:54", 1),
            good.replacen(":3:54", ":3:9223372036854775808", 1),
        ];
        for bytes in bad {
            assert!(Manifest::parse(bytes.as_bytes()).is_err());
        }
        let mut duplicate = doc.clone();
        duplicate.rows.push(doc.rows[0].clone());
        assert!(duplicate.serialize().is_err());
        let duplicate_raw = format!(
            "4\n0:{DOC}:2:34\n{0}:0:{DOC}.content:0:17\n{0}:0:{DOC}.content:0:17\n",
            doc.rows[0].hash
        );
        assert!(Manifest::parse(duplicate_raw.as_bytes()).is_err());
        for invalid in ["", "a:b", "a\nb", "a\0b"] {
            let mut bad = doc.clone();
            bad.rows[0].name = invalid.into();
            assert!(bad.serialize().is_err());
        }
        let mut bad = doc;
        bad.rows[0].hash = "AB".repeat(32);
        assert!(bad.serialize().is_err());
    }
    #[test]
    fn rejects_aggregate_overflow() {
        let mut doc = document();
        doc.rows[0].size = i64::MAX as u64;
        assert!(doc.serialize().is_err());
    }
    #[test]
    fn new_job_has_leaf_counts_and_sizes_not_manifest_length() {
        let doc = document();
        let job = crate::job::Job {
            version: 1,
            uuid: uuid::Uuid::parse_str(DOC).unwrap(),
            entry_hash: "00".repeat(32),
            files: doc
                .rows
                .iter()
                .map(|r| crate::job::Part {
                    path: r.name.clone(),
                    hash: r.hash.clone(),
                    size: r.size,
                })
                .collect(),
            state: crate::job::State::Prepared,
            completed_generation: None,
            verified_library_generation: None,
            hash_tree: None,
        };
        let created = Manifest::from_job(&job).unwrap();
        assert_eq!(created, doc);
        assert!(created.rows.iter().all(|r| r.flags == 0 && r.count == 0));
        assert_eq!(created.total_size().unwrap(), 54);
        assert_ne!(
            created.total_size().unwrap(),
            created.serialize().unwrap().len() as u64
        );
    }
}
