//! The format file tests.
//!
//! This test looks for directories in `tests/format`.
//!
//! Each directory is expected to contain:
//!
//! * `source.wdl` - the test input source to parse.
//! * `source.formatted.wdl` - the expected formatted output.
//!
//! The `source.formatted.wdl` file may be automatically generated or updated by
//! setting the `BLESS` environment variable when running this test.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codespan_reporting::files::SimpleFile;
use codespan_reporting::term;
use codespan_reporting::term::Config;
use codespan_reporting::term::termcolor::Buffer;
use colored::Colorize;
use pretty_assertions::StrComparison;
use rayon::prelude::*;
use wdl_ast::Diagnostic;
use wdl_ast::Document;
use wdl_ast::Node;
use wdl_format::Formatter;
use wdl_format::element::FormatElement;
use wdl_format::element::node::AstNodeFormatExt;

/// Normalizes a result.
fn normalize(s: &str) -> String {
    // Just normalize line endings
    s.replace("\r\n", "\n")
}

/// Find all the tests in the `tests/format` directory.
fn find_tests() -> Vec<PathBuf> {
    // Check for filter arguments consisting of test names
    let mut filter = HashSet::new();
    for arg in std::env::args().skip_while(|a| a != "--").skip(1) {
        if !arg.starts_with('-') {
            filter.insert(arg);
        }
    }

    let mut tests: Vec<PathBuf> = Vec::new();
    for entry in Path::new("tests/format").read_dir().unwrap() {
        let entry = entry.expect("failed to read directory");
        let path = entry.path();
        if !path.is_dir()
            || (!filter.is_empty()
                && !filter.contains(entry.file_name().to_str().expect("name should be UTF-8")))
        {
            continue;
        }

        tests.push(path);
    }

    tests.sort();
    tests
}

/// Format a list of diagnostics.
fn format_diagnostics(diagnostics: &[Diagnostic], path: &Path, source: &str) -> String {
    let file = SimpleFile::new(path.as_os_str().to_str().unwrap(), source);
    let mut buffer = Buffer::no_color();
    for diagnostic in diagnostics {
        term::emit(
            &mut buffer,
            &Config::default(),
            &file,
            &diagnostic.to_codespan(()),
        )
        .expect("should emit");
    }

    String::from_utf8(buffer.into_inner()).expect("should be UTF-8")
}

/// Compare the result of a test to the expected result.
fn compare_result(path: &Path, result: &str) -> Result<(), String> {
    let result = normalize(result);
    if env::var_os("BLESS").is_some() {
        fs::write(path, &result).map_err(|e| {
            format!(
                "failed to write result file `{path}`: {e}",
                path = path.display()
            )
        })?;
        return Ok(());
    }

    let expected = fs::read_to_string(path)
        .map_err(|e| {
            format!(
                "failed to read result file `{path}`: {e}",
                path = path.display()
            )
        })?
        .replace("\r\n", "\n");

    if expected != result {
        return Err(format!(
            "result from `{path}` is not as expected:\n{diff}",
            path = path.display(),
            diff = StrComparison::new(&expected, &result),
        ));
    }

    Ok(())
}

/// Parses source string into a document FormatElement
fn prepare_document(source: &str, path: &Path) -> Result<FormatElement, String> {
    let (document, diagnostics) = Document::parse(source);

    if !diagnostics.is_empty() {
        return Err(format!(
            "failed to parse `{path}` {e}",
            path = path.display(),
            e = format_diagnostics(&diagnostics, path, source)
        ));
    };

    Ok(Node::Ast(document.ast().into_v1().unwrap()).into_format_element())
}

/// Parses and formats source string
fn format(source: &str, path: &Path) -> Result<String, String> {
    let document = prepare_document(source, path)?;
    let formatted = match Formatter::default().format(&document) {
        Ok(formatted) => formatted,
        Err(e) => {
            return Err(format!(
                "failed to format `{path}`: {e}",
                path = path.display(),
                e = e
            ));
        }
    };
    Ok(formatted)
}

/// Run a test.
fn run_test(test: &Path, ntests: &AtomicUsize) -> Result<(), String> {
    let path = test.join("source.wdl");
    let formatted_path = path.with_extension("formatted.wdl");
    let source = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "failed to read source file `{path}`: {e}",
            path = path.display()
        )
    })?;

    let formatted = format(&source, path.as_path())?;
    compare_result(formatted_path.as_path(), &formatted)?;

    // test idempotency by formatting the formatted document
    let twice_formatted = format(&formatted, formatted_path.as_path())?;
    compare_result(formatted_path.as_path(), &twice_formatted)?;

    ntests.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Run all the tests.
fn main() {
    let tests = find_tests();
    println!("\nrunning {} tests\n", tests.len());

    let ntests = AtomicUsize::new(0);
    let errors = tests
        .par_iter()
        .filter_map(|test| {
            let test_name = test.file_stem().and_then(OsStr::to_str).unwrap();
            match std::panic::catch_unwind(|| {
                match run_test(test, &ntests)
                    .map_err(|e| format!("failed to run test `{path}`: {e}", path = test.display()))
                    .err()
                {
                    Some(e) => {
                        println!("test {test_name} ... {failed}", failed = "failed".red());
                        Some((test_name, e))
                    }
                    None => {
                        println!("test {test_name} ... {ok}", ok = "ok".green());
                        None
                    }
                }
            }) {
                Ok(result) => result,
                Err(e) => {
                    println!(
                        "test {test_name} ... {panicked}",
                        panicked = "panicked".red()
                    );
                    Some((
                        test_name,
                        format!(
                            "test panicked: {e:?}",
                            e = e
                                .downcast_ref::<String>()
                                .map(|s| s.as_str())
                                .or_else(|| e.downcast_ref::<&str>().copied())
                                .unwrap_or("no panic message")
                        ),
                    ))
                }
            }
        })
        .collect::<Vec<_>>();

    if !errors.is_empty() {
        eprintln!(
            "\n{count} test(s) {failed}:",
            count = errors.len(),
            failed = "failed".red()
        );

        for (name, msg) in errors.iter() {
            eprintln!("{name}: {msg}", msg = msg.red());
        }

        exit(1);
    }

    println!(
        "\ntest result: ok. {} passed\n",
        ntests.load(Ordering::SeqCst)
    );
}
