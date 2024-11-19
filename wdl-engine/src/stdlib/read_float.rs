//! Implements the `read_float` function from the WDL standard library.

use std::fs;
use std::io::BufRead;
use std::io::BufReader;

use wdl_analysis::types::PrimitiveTypeKind;
use wdl_ast::Diagnostic;

use super::CallContext;
use super::Function;
use super::Signature;
use crate::Value;
use crate::diagnostics::function_call_failed;

/// Reads a file that contains only a float value and (optional) whitespace.
///
/// If the line contains a valid floating point number, that value is returned
/// as a Float. If the file is empty or does not contain a single float, an
/// error is raised.
///
/// https://github.com/openwdl/wdl/blob/wdl-1.2/SPEC.md#read_float
fn read_float(context: CallContext<'_>) -> Result<Value, Diagnostic> {
    debug_assert!(context.arguments.len() == 1);
    debug_assert!(context.return_type_eq(PrimitiveTypeKind::Float));

    let path = context.cwd().join(
        context
            .coerce_argument(0, PrimitiveTypeKind::File)
            .unwrap_file()
            .as_str(),
    );

    let read_error = |e: std::io::Error| {
        function_call_failed(
            "read_float",
            format!("failed to read file `{path}`: {e}", path = path.display()),
            context.call_site,
        )
    };

    let invalid_contents = || {
        function_call_failed(
            "read_float",
            format!(
                "file `{path}` does not contain a float value on a single line",
                path = path.display()
            ),
            context.call_site,
        )
    };

    let mut lines = BufReader::new(fs::File::open(&path).map_err(read_error)?).lines();
    let line = lines
        .next()
        .ok_or_else(invalid_contents)?
        .map_err(read_error)?;

    if lines.next().is_some() {
        return Err(invalid_contents());
    }

    Ok(line
        .trim()
        .parse::<f64>()
        .map_err(|_| invalid_contents())?
        .into())
}

/// Gets the function describing `read_float`.
pub const fn descriptor() -> Function {
    Function::new(const { &[Signature::new("(File) -> Float", read_float)] })
}

#[cfg(test)]
mod test {
    use wdl_ast::version::V1;

    use crate::PrimitiveValue;
    use crate::v1::test::TestEnv;
    use crate::v1::test::eval_v1_expr;

    #[test]
    fn read_float() {
        let mut env = TestEnv::default();
        env.write_file("foo", "12345.6789 hello world!");
        env.write_file("bar", "\t \t 12345.6789   \n");
        env.insert_name("file", PrimitiveValue::new_file("bar"));

        let diagnostic =
            eval_v1_expr(&mut env, V1::Two, "read_float('does-not-exist')").unwrap_err();
        assert!(
            diagnostic
                .message()
                .starts_with("call to function `read_float` failed: failed to read file")
        );

        let diagnostic = eval_v1_expr(&mut env, V1::Two, "read_float('foo')").unwrap_err();
        assert!(
            diagnostic
                .message()
                .contains("does not contain a float value on a single line")
        );

        let value = eval_v1_expr(&mut env, V1::Two, "read_float('bar')").unwrap();
        approx::assert_relative_eq!(value.unwrap_float(), 12345.6789);

        let value = eval_v1_expr(&mut env, V1::Two, "read_float(file)").unwrap();
        approx::assert_relative_eq!(value.unwrap_float(), 12345.6789);
    }
}
