use std::fmt::{Debug, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use lazy_static::lazy_static;
use nom::IResult;
use nom_locate::LocatedSpan;
use regex::Regex;

use crate::MigrateError;

lazy_static! {
    static ref FILENAME_REGEX: Regex =
        regex::Regex::new(r"^[Vv](?P<version>[\d]{3})(?:[-_\da-zA-Z]*)?.cql$")
            .expect("cql filename regex");
}

#[derive(Clone, Debug)]
pub struct CqlFile {
    pub filename: String,
    pub hash: String,
    pub path: PathBuf,
    pub version: i16,
}

#[derive(Debug)]
pub struct CqlStatement {
    pub cql: String,
    pub lines: (usize, usize),
}

impl Display for CqlFile {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.filename)
    }
}

impl CqlFile {
    pub fn from_path(path: PathBuf) -> Result<CqlFile> {
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        if !FILENAME_REGEX.is_match(filename.as_str()) {
            // todo use MigrateError for main.rs to handle error with
            //  info about _ for .cql files to be omitted from migrate
            return Err(anyhow!("{filename} is not a valid cql file name"));
        }
        let hash = match fs::read(&path) {
            Err(err) => return Err(anyhow!("failed reading file {}: {err}", filename)),
            Ok(file_content) => format!("{:x}", md5::compute(file_content)),
        };
        let version = FILENAME_REGEX
            .captures(filename.as_str())
            .unwrap()
            .name("version")
            .unwrap()
            .as_str();
        let version = version.parse::<i16>().with_context(|| {
            format!("Migration file `{filename}` has invalid version `{version}`")
        })?;
        Ok(CqlFile {
            filename,
            hash,
            path,
            version,
        })
    }

    pub(crate) fn read_statements(&self) -> Result<Vec<CqlStatement>, MigrateError> {
        use nom::{
            branch::alt,
            bytes::complete::tag,
            character::complete::{anychar, char, line_ending, one_of},
            combinator::{all_consuming, map, recognize},
            error::{context, Error},
            multi::{many1, many_till},
            sequence::preceded,
            Parser,
        };

        let cql = fs::read(&self.path).map_err(|e| MigrateError::CqlFileReadError {
            filename: self.path.to_string_lossy().into(),
            error: e.to_string(),
        })?;
        let cql = String::from_utf8(cql).map_err(|e| MigrateError::CqlFileReadError {
            filename: self.path.to_string_lossy().into(),
            error: format!("CQL file contains invalid UTF-8 sequence: {e}"),
        })?;

        let parse_result: IResult<LocatedSpan<&str>, Vec<Option<CqlStatement>>, Error<_>> =
            all_consuming(many1(alt((
                // eat whitespace
                context("read whitespace", map(many1(one_of("\n\r ")), |_| None)),
                // line comment
                context(
                    "read line comment",
                    map(preceded(tag("--"), many_till(anychar, line_ending)), |_| {
                        None
                    }),
                ),
                // block comment
                context(
                    "read block comment",
                    map(preceded(tag("/*"), many_till(anychar, tag("*/"))), |_| None),
                ),
                // actual statement
                context(
                    "read statement",
                    map(
                        recognize(many_till(anychar, char(';'))),
                        |lb: LocatedSpan<&str>| {
                            let open_line = lb.location_line() as usize;
                            let cql: String = lb.into_fragment().into();
                            let line_count = cql.lines().count();
                            Some(CqlStatement {
                                lines: (open_line, open_line + line_count),
                                cql,
                            })
                        },
                    ),
                ),
            ))))
            .parse_complete(LocatedSpan::new(cql.as_str()));

        // code _somewhere_ could actually iterate over a VerboseError
        // from nom-language; however, it borrows the input text, and
        // I do not care to make an elegant interface to propagate
        // that information at this time.

        let (_, statements) = parse_result.map_err(|e| MigrateError::CqlFileReadError {
            filename: self.path.to_string_lossy().into(),
            error: format!("Unable to parse migration file: {e}"),
        })?;

        Ok(statements.into_iter().filter_map(|i| i).collect())
    }
}

pub(crate) fn files_from_dir(cql_dir: &PathBuf) -> Result<Vec<CqlFile>> {
    let cql_file_paths =
        read_cql_file_paths(cql_dir).context("Failed to scan migration directory")?;
    let mut cql_files: Vec<CqlFile> = Vec::with_capacity(cql_file_paths.len());
    let mut expected_version: i16 = 1;
    for path in cql_file_paths {
        let cql_file = CqlFile::from_path(path.clone()).with_context(|| {
            format!("Unable to create valid migration metadata for file `{path:?}`")
        })?;
        if cql_file.version != expected_version {
            return if cql_file.version == expected_version - 1 {
                let previous_index =
                    usize::try_from(expected_version - 2).context("Previous index invalid")?;
                let previous_filename = &cql_files.get(previous_index).unwrap().filename;
                Err(anyhow!(
                    "{} and {} repeat versions instead of incrementing to v{:0>3}",
                    previous_filename,
                    cql_file.filename,
                    expected_version
                ))
            } else {
                Err(anyhow!(
                    "{} found without a preceding v{:0>3} version cql file",
                    cql_file.filename,
                    expected_version
                ))
            };
        }
        cql_files.push(cql_file);
        expected_version += 1;
    }
    Ok(cql_files)
}

fn read_cql_file_paths(cql_dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let dir_read = match fs::read_dir(cql_dir) {
        Err(_) => {
            return Err(anyhow!(
                "could not find directory '{}'",
                cql_dir.to_string_lossy()
            ));
        }
        Ok(dir_read) => dir_read,
    };
    let mut cql_file_paths = Vec::new();
    for dir_entry in dir_read {
        let path = dir_entry?.path();
        if is_inclusive_cql_filename(&path) {
            cql_file_paths.push(path);
        }
    }
    cql_file_paths.sort();
    if cql_file_paths.is_empty() {
        return Err(anyhow!(
            "no cql files found in directory '{}'",
            cql_dir.to_string_lossy()
        ));
    }
    Ok(cql_file_paths)
}

fn is_inclusive_cql_filename(path: &Path) -> bool {
    if path.is_file() {
        if let Some(file_name) = path.file_name() {
            if let Some(extension) = path.extension() {
                if extension == "cql" && !file_name.to_string_lossy().starts_with('_') {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use temp_dir::TempDir;

    use crate::test_utils::make_file;

    use super::*;

    #[test]
    fn test_cql_filename_regex() {
        assert!(FILENAME_REGEX.is_match("v000.cql"));
        assert!(FILENAME_REGEX.is_match("v001-init-schema.cql"));
        assert!(FILENAME_REGEX.is_match("V002_add_column_families.cql"));
        assert!(!FILENAME_REGEX.is_match("init-schema.cql"));
    }

    #[test]
    fn test_cql_file() {
        let temp_dir = TempDir::new().unwrap();
        let cql_file_path = temp_dir.path().join("v073-more_tables.cql");
        make_file(
            cql_file_path.clone(),
            "  create table big_business_data (id timeuuid primary key)  ;",
        );

        match CqlFile::from_path(cql_file_path) {
            Err(_) => panic!(),
            Ok(cql_file) => {
                assert_eq!(cql_file.filename, String::from("v073-more_tables.cql"));
                assert_eq!(cql_file.version, 73);
                assert_eq!(cql_file.hash, "e995c628cf1a06863dc86760020ecb43");
                let statements_result = cql_file.read_statements();
                assert!(statements_result.is_ok());
                let statements = statements_result.unwrap();
                assert_eq!(statements.len(), 1);
                assert_eq!(
                    statements.get(0).unwrap().cql,
                    "create table big_business_data (id timeuuid primary key)"
                );
            }
        }
    }

    fn read_statements_test(cql: &'static str, expected: Vec<CqlStatement>) {
        let temp_dir = TempDir::new().unwrap();
        let cql_file_path = temp_dir.path().join("v001-no_more_tests.cql");
        make_file(cql_file_path.clone(), cql);
        let cql_file = CqlFile::from_path(cql_file_path).expect("cql file");
        let statements_result = cql_file.read_statements();
        assert!(statements_result.is_ok());
        let statements = statements_result.unwrap();
        assert_eq!(statements.len(), expected.len());
        for (i, statement) in statements.iter().enumerate() {
            let other = expected.get(i).unwrap();
            assert_eq!(statement.cql, other.cql);
            assert_eq!(
                statement.lines.0,
                other.lines.0,
                "{}",
                statement.cql.as_str()
            );
            assert_eq!(
                statement.lines.1,
                other.lines.1,
                "{}",
                statement.cql.as_str()
            );
        }
    }

    #[test]
    fn test_cql_file_read_statements_incomplete_line() {
        read_statements_test(
            "create table big_business_data (id timeuuid primary key)",
            Vec::new(),
        );
    }

    #[test]
    fn test_cql_file_read_statements_complete_line() {
        read_statements_test(
            "create table big_business_data (id timeuuid primary key);",
            Vec::from([CqlStatement {
                cql: "create table big_business_data (id timeuuid primary key)".to_string(),
                lines: (1, 1),
            }]),
        );
    }

    #[test]
    fn test_cql_file_read_statements_two_lines() {
        read_statements_test(
            "create table big_business_data (id timeuuid primary key);
                      create table more_business_data (id timeuuid primary key);",
            Vec::from([
                CqlStatement {
                    cql: "create table big_business_data (id timeuuid primary key)".to_string(),
                    lines: (1, 1),
                },
                CqlStatement {
                    cql: "create table more_business_data (id timeuuid primary key)".to_string(),
                    lines: (2, 2),
                },
            ]),
        );
    }

    #[test]
    fn test_cql_file_read_statements_block_comment_only() {
        read_statements_test(
            "/*create table big_business_data (id timeuuid primary key);*/",
            Vec::new(),
        );
    }

    #[test]
    fn test_cql_file_read_statements_line_comment_only() {
        read_statements_test(
            "--create table big_business_data (id timeuuid primary key);",
            Vec::new(),
        );
    }

    #[test]
    fn test_cql_file_read_statements_multiline_statement_with_line_comments() {
        read_statements_test(
            "create table big_business_data (
            id timeuuid primary key,
            -- here's some docs
            data text, -- and more docs
            created timestamp
            );",
            vec![CqlStatement {
                cql: "create table big_business_data (id timeuuid primary key, data text, created timestamp)"
                    .to_string(),
                lines: (1, 6),
            }],
        );
    }

    #[test]
    fn test_cql_file_read_statements_block_comment_in_statement() {
        read_statements_test(
            "create table big_business_data (
            /*id timeuuid primary key,*/
            another_id uuid primary key,
            /*data text,*/
            data text
            );",
            vec![CqlStatement {
                cql: "create table big_business_data ( another_id uuid primary key, data text)"
                    .to_string(),
                lines: (1, 6),
            }],
        );
    }

    #[test]
    fn test_cql_file_read_statements_line_comment_out_statement_ending() {
        read_statements_test(
            "create table big_business_data (--id timeuuid primary key);\nanother_id uuid primary key);",
            vec!(
                CqlStatement {
                    cql: "create table big_business_data ( another_id uuid primary key)".to_string(),
                    lines: (1, 2),
                })
        );
    }

    #[test]
    fn test_cql_file_read_statements_block_comment_between_statements() {
        read_statements_test(
            "create table big_business_data (id timeuuid primary key);
            /*create table another_business_data (id timeuuid primary key);*/
            create table more_business_data (id timeuuid primary key);",
            vec![
                CqlStatement {
                    cql: "create table big_business_data (id timeuuid primary key)".to_string(),
                    lines: (1, 1),
                },
                CqlStatement {
                    cql: "create table more_business_data (id timeuuid primary key)".to_string(),
                    lines: (3, 3),
                },
            ],
        );
    }

    #[test]
    fn test_cql_file_read_statements_line_comment_between_statements() {
        read_statements_test(
            "create table big_business_data (id timeuuid primary key);
            --create table another_business_data (id timeuuid primary key);
            create table more_business_data (id timeuuid primary key);",
            vec![
                CqlStatement {
                    cql: "create table big_business_data (id timeuuid primary key)".to_string(),
                    lines: (1, 1),
                },
                CqlStatement {
                    cql: "create table more_business_data (id timeuuid primary key)".to_string(),
                    lines: (3, 3),
                },
            ],
        );
    }

    #[test]
    fn test_cql_file_read_statements_partial_line_comment_between_statements() {
        read_statements_test(
            "create table big_business_data (id timeuuid primary key);
            create table --another_business_data (id timeuuid primary key);
            more_business_data (id timeuuid primary key);
            create table even_more_business_data (id timeuuid primary key);",
            vec![
                CqlStatement {
                    cql: "create table big_business_data (id timeuuid primary key)".to_string(),
                    lines: (1, 1),
                },
                CqlStatement {
                    cql: "create table more_business_data (id timeuuid primary key)".to_string(),
                    lines: (2, 3),
                },
                CqlStatement {
                    cql: "create table even_more_business_data (id timeuuid primary key)"
                        .to_string(),
                    lines: (4, 4),
                },
            ],
        );
    }

    #[test]
    fn test_files_from_dir() {
        let temp_dir = TempDir::new().unwrap();
        ["v001.cql", "foo.sh", "foo.sql"]
            .iter()
            .for_each(|f| make_file(temp_dir.path().join(f), ""));
        let temp_dir_path = temp_dir.path().canonicalize().unwrap();

        match files_from_dir(&temp_dir_path) {
            Err(err) => {
                println!("{err}");
                panic!();
            }
            Ok(cql_files) => {
                assert_eq!(cql_files.len(), 1);
                assert!(cql_files.iter().any(|p| { p.filename == "v001.cql" }));
            }
        }
    }

    #[test]
    fn test_files_from_dir_errors_with_cql_name() {
        let temp_dir = TempDir::new().unwrap();
        ["foo.cql"]
            .iter()
            .for_each(|f| make_file(temp_dir.path().join(f), ""));
        let temp_dir_path = temp_dir.path().canonicalize().unwrap();

        match files_from_dir(&temp_dir_path) {
            Ok(_) => panic!(),
            Err(err) => {
                assert_eq!(err.to_string(), "foo.cql is not a valid cql file name");
            }
        }
    }

    #[test]
    fn test_files_from_dir_allows_non_migrating_cql() {
        let temp_dir = TempDir::new().unwrap();
        ["v001-foo.cql", "_foo.cql"]
            .iter()
            .for_each(|f| make_file(temp_dir.path().join(f), ""));
        let temp_dir_path = temp_dir.path().canonicalize().unwrap();

        match files_from_dir(&temp_dir_path) {
            Ok(cql_files) => {
                assert_eq!(cql_files.len(), 1);
                assert_eq!(
                    cql_files.get(0).unwrap().filename,
                    "v001-foo.cql".to_string()
                );
            }
            Err(err) => panic!("should not have errored with: {err}"),
        }
    }

    #[test]
    fn test_files_from_dir_errors_with_out_of_order_versions() {
        let temp_dir = TempDir::new().unwrap();
        ["v001-foo.cql", "v002-foo.cql", "v004-foo.cql"]
            .iter()
            .for_each(|f| make_file(temp_dir.path().join(f), ""));
        let temp_dir_path = temp_dir.path().canonicalize().unwrap();

        match files_from_dir(&temp_dir_path) {
            Ok(_) => panic!("cql::files_from_dir should have errored"),
            Err(err) => {
                assert_eq!(
                    err.to_string(),
                    "v004-foo.cql found without a preceding v003 version cql file"
                );
            }
        }
    }

    #[test]
    fn test_files_from_dir_errors_with_repeating_versions() {
        let temp_dir = TempDir::new().unwrap();
        ["v001-foo.cql", "v001-bar.cql"]
            .iter()
            .for_each(|f| make_file(temp_dir.path().join(f), ""));
        let temp_dir_path = temp_dir.path().canonicalize().unwrap();

        match files_from_dir(&temp_dir_path) {
            Ok(_) => panic!("cql::files_from_dir should have errored"),
            Err(err) => {
                assert_eq!(
                    err.to_string(),
                    "v001-bar.cql and v001-foo.cql repeat versions instead of incrementing to v002"
                );
            }
        }
    }
}
