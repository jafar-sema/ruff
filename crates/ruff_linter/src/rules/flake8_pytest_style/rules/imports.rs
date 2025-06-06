use ruff_python_ast::Stmt;

use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_text_size::Ranged;

use crate::{Violation, checkers::ast::Checker};

/// ## What it does
/// Checks for incorrect import of pytest.
///
/// ## Why is this bad?
/// For consistency, `pytest` should be imported as `import pytest` and its members should be
/// accessed in the form of `pytest.xxx.yyy` for consistency
///
/// ## Example
/// ```python
/// import pytest as pt
/// from pytest import fixture
/// ```
///
/// Use instead:
/// ```python
/// import pytest
/// ```
#[derive(ViolationMetadata)]
pub(crate) struct PytestIncorrectPytestImport;

impl Violation for PytestIncorrectPytestImport {
    #[derive_message_formats]
    fn message(&self) -> String {
        "Incorrect import of `pytest`; use `import pytest` instead".to_string()
    }
}

fn is_pytest_or_subpackage(imported_name: &str) -> bool {
    imported_name == "pytest" || imported_name.starts_with("pytest.")
}

/// PT013
pub(crate) fn import(checker: &Checker, import_from: &Stmt, name: &str, asname: Option<&str>) {
    if is_pytest_or_subpackage(name) {
        if let Some(alias) = asname {
            if alias != name {
                checker.report_diagnostic(PytestIncorrectPytestImport, import_from.range());
            }
        }
    }
}

/// PT013
pub(crate) fn import_from(checker: &Checker, import_from: &Stmt, module: Option<&str>, level: u32) {
    // If level is not zero or module is none, return
    if level != 0 {
        return;
    }

    if let Some(module) = module {
        if is_pytest_or_subpackage(module) {
            checker.report_diagnostic(PytestIncorrectPytestImport, import_from.range());
        }
    }
}
