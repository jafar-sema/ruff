use anyhow::{Context, anyhow};
use ruff_db::Upcast;
use ruff_db::files::{File, Files, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::system::{DbWithTestSystem, System, SystemPath, SystemPathBuf, TestSystem};
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::visitor::source_order;
use ruff_python_ast::visitor::source_order::SourceOrderVisitor;
use ruff_python_ast::{
    self as ast, Alias, Comprehension, Expr, Parameter, ParameterWithDefault, PythonVersion, Stmt,
};

use ty_python_semantic::lint::{LintRegistry, RuleSelection};
use ty_python_semantic::{
    Db, HasType, Program, ProgramSettings, PythonPlatform, PythonVersionSource,
    PythonVersionWithSource, SearchPathSettings, SemanticModel, default_lint_registry,
};

fn get_cargo_workspace_root() -> anyhow::Result<SystemPathBuf> {
    Ok(SystemPathBuf::from(String::from_utf8(
        std::process::Command::new("cargo")
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .output()?
            .stdout,
    )?)
    .parent()
    .unwrap()
    .to_owned())
}

/// Test that all snippets in testcorpus can be checked without panic (except for [`KNOWN_FAILURES`])
#[test]
fn corpus_no_panic() -> anyhow::Result<()> {
    let crate_root = String::from(env!("CARGO_MANIFEST_DIR"));
    run_corpus_tests(&format!("{crate_root}/resources/corpus/**/*.py"))
}

#[test]
fn parser_no_panic() -> anyhow::Result<()> {
    let workspace_root = get_cargo_workspace_root()?;
    run_corpus_tests(&format!(
        "{workspace_root}/crates/ruff_python_parser/resources/**/*.py"
    ))
}

#[test]
fn linter_af_no_panic() -> anyhow::Result<()> {
    let workspace_root = get_cargo_workspace_root()?;
    run_corpus_tests(&format!(
        "{workspace_root}/crates/ruff_linter/resources/test/fixtures/[a-f]*/**/*.py"
    ))
}

#[test]
fn linter_gz_no_panic() -> anyhow::Result<()> {
    let workspace_root = get_cargo_workspace_root()?;
    run_corpus_tests(&format!(
        "{workspace_root}/crates/ruff_linter/resources/test/fixtures/[g-z]*/**/*.py"
    ))
}

#[test]
fn linter_stubs_no_panic() -> anyhow::Result<()> {
    let workspace_root = get_cargo_workspace_root()?;
    run_corpus_tests(&format!(
        "{workspace_root}/crates/ruff_linter/resources/test/fixtures/**/*.pyi"
    ))
}

#[test]
fn typeshed_no_panic() -> anyhow::Result<()> {
    let workspace_root = get_cargo_workspace_root()?;
    run_corpus_tests(&format!(
        "{workspace_root}/crates/ty_vendored/vendor/typeshed/**/*.pyi"
    ))
}

#[expect(clippy::print_stdout)]
fn run_corpus_tests(pattern: &str) -> anyhow::Result<()> {
    let root = SystemPathBuf::from("/src");

    let mut db = CorpusDb::new();
    db.memory_file_system()
        .create_directory_all(root.as_ref())?;

    let workspace_root = get_cargo_workspace_root()?;
    let workspace_root = workspace_root.to_string();

    let corpus = glob::glob(pattern).context("Failed to compile pattern")?;

    for path in corpus {
        let path = path.context("Failed to glob path")?;
        let path = SystemPathBuf::from_path_buf(path).map_err(|path| {
            anyhow!(
                "Failed to convert path '{path}' to system path",
                path = path.display()
            )
        })?;

        let relative_path = path.strip_prefix(&workspace_root)?;

        let (py_expected_to_fail, pyi_expected_to_fail) = KNOWN_FAILURES
            .iter()
            .find_map(|(path, py_fail, pyi_fail)| {
                if *path == relative_path.as_str().replace('\\', "/") {
                    Some((*py_fail, *pyi_fail))
                } else {
                    None
                }
            })
            .unwrap_or((false, false));

        let source = path.as_path();
        let source_filename = source.file_name().unwrap();

        let code = std::fs::read_to_string(source)
            .with_context(|| format!("Failed to read test file: {path}"))?;

        let mut check_with_file_name = |path: &SystemPath| {
            if relative_path.file_name() == Some("types.pyi") {
                println!(
                    "Skipping {relative_path:?}: paths with `types.pyi` as their final segment cause a stack overflow"
                );
                return;
            }

            db.memory_file_system().write_file_all(path, &code).unwrap();
            File::sync_path(&mut db, path);

            // this test is only asserting that we can pull every expression type without a panic
            // (and some non-expressions that clearly define a single type)
            let file = system_path_to_file(&db, path).unwrap();

            let result = std::panic::catch_unwind(|| pull_types(&db, file));

            let expected_to_fail = if path.extension().map(|e| e == "pyi").unwrap_or(false) {
                pyi_expected_to_fail
            } else {
                py_expected_to_fail
            };
            if let Err(err) = result {
                if !expected_to_fail {
                    println!(
                        "Check failed for {relative_path:?}. Consider fixing it or adding it to KNOWN_FAILURES"
                    );
                    std::panic::resume_unwind(err);
                }
            } else {
                assert!(
                    !expected_to_fail,
                    "Expected to panic, but did not. Consider removing this path from KNOWN_FAILURES"
                );
            }

            db.memory_file_system().remove_file(path).unwrap();
            file.sync(&mut db);
        };

        if source.extension() == Some("pyi") {
            println!("checking {relative_path}");
            let pyi_dest = root.join(source_filename);
            check_with_file_name(&pyi_dest);
        } else {
            println!("checking {relative_path}");
            let py_dest = root.join(source_filename);
            check_with_file_name(&py_dest);

            let pyi_dest = root.join(format!("{source_filename}i"));
            println!("re-checking as stub file: {pyi_dest}");
            check_with_file_name(&pyi_dest);
        }
    }

    Ok(())
}

fn pull_types(db: &dyn Db, file: File) {
    let mut visitor = PullTypesVisitor::new(db, file);

    let ast = parsed_module(db.upcast(), file).load(db.upcast());

    visitor.visit_body(ast.suite());
}

struct PullTypesVisitor<'db> {
    model: SemanticModel<'db>,
}

impl<'db> PullTypesVisitor<'db> {
    fn new(db: &'db dyn Db, file: File) -> Self {
        Self {
            model: SemanticModel::new(db, file),
        }
    }

    fn visit_target(&mut self, target: &Expr) {
        match target {
            Expr::List(ast::ExprList { elts, .. }) | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for element in elts {
                    self.visit_target(element);
                }
            }
            _ => self.visit_expr(target),
        }
    }
}

impl SourceOrderVisitor<'_> for PullTypesVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                let _ty = function.inferred_type(&self.model);
            }
            Stmt::ClassDef(class) => {
                let _ty = class.inferred_type(&self.model);
            }
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    self.visit_target(target);
                }
                self.visit_expr(&assign.value);
                return;
            }
            Stmt::For(for_stmt) => {
                self.visit_target(&for_stmt.target);
                self.visit_expr(&for_stmt.iter);
                self.visit_body(&for_stmt.body);
                self.visit_body(&for_stmt.orelse);
                return;
            }
            Stmt::With(with_stmt) => {
                for item in &with_stmt.items {
                    if let Some(target) = &item.optional_vars {
                        self.visit_target(target);
                    }
                    self.visit_expr(&item.context_expr);
                }

                self.visit_body(&with_stmt.body);
                return;
            }
            Stmt::AnnAssign(_)
            | Stmt::Return(_)
            | Stmt::Delete(_)
            | Stmt::AugAssign(_)
            | Stmt::TypeAlias(_)
            | Stmt::While(_)
            | Stmt::If(_)
            | Stmt::Match(_)
            | Stmt::Raise(_)
            | Stmt::Try(_)
            | Stmt::Assert(_)
            | Stmt::Import(_)
            | Stmt::ImportFrom(_)
            | Stmt::Global(_)
            | Stmt::Nonlocal(_)
            | Stmt::Expr(_)
            | Stmt::Pass(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::IpyEscapeCommand(_) => {}
        }

        source_order::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        let _ty = expr.inferred_type(&self.model);

        source_order::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, comprehension: &Comprehension) {
        self.visit_expr(&comprehension.iter);
        self.visit_target(&comprehension.target);
        for if_expr in &comprehension.ifs {
            self.visit_expr(if_expr);
        }
    }

    fn visit_parameter(&mut self, parameter: &Parameter) {
        let _ty = parameter.inferred_type(&self.model);

        source_order::walk_parameter(self, parameter);
    }

    fn visit_parameter_with_default(&mut self, parameter_with_default: &ParameterWithDefault) {
        let _ty = parameter_with_default.inferred_type(&self.model);

        source_order::walk_parameter_with_default(self, parameter_with_default);
    }

    fn visit_alias(&mut self, alias: &Alias) {
        let _ty = alias.inferred_type(&self.model);

        source_order::walk_alias(self, alias);
    }
}

/// Whether or not the .py/.pyi version of this file is expected to fail
#[rustfmt::skip]
const KNOWN_FAILURES: &[(&str, bool, bool)] = &[
    // Fails with too-many-cycle-iterations due to a self-referential
    // type alias, see https://github.com/astral-sh/ty/issues/256
    ("crates/ruff_linter/resources/test/fixtures/pyflakes/F401_34.py", true, true),
];

#[salsa::db]
#[derive(Clone)]
pub struct CorpusDb {
    storage: salsa::Storage<Self>,
    files: Files,
    rule_selection: RuleSelection,
    system: TestSystem,
    vendored: VendoredFileSystem,
}

impl CorpusDb {
    #[expect(clippy::new_without_default)]
    pub fn new() -> Self {
        let db = Self {
            storage: salsa::Storage::new(None),
            system: TestSystem::default(),
            vendored: ty_vendored::file_system().clone(),
            rule_selection: RuleSelection::from_registry(default_lint_registry()),
            files: Files::default(),
        };

        Program::from_settings(
            &db,
            ProgramSettings {
                python_version: Some(PythonVersionWithSource {
                    version: PythonVersion::latest_ty(),
                    source: PythonVersionSource::default(),
                }),
                python_platform: PythonPlatform::default(),
                search_paths: SearchPathSettings::new(vec![]),
            },
        )
        .unwrap();

        db
    }
}

impl DbWithTestSystem for CorpusDb {
    fn test_system(&self) -> &TestSystem {
        &self.system
    }

    fn test_system_mut(&mut self) -> &mut TestSystem {
        &mut self.system
    }
}

#[salsa::db]
impl ruff_db::Db for CorpusDb {
    fn vendored(&self) -> &VendoredFileSystem {
        &self.vendored
    }

    fn system(&self) -> &dyn System {
        &self.system
    }

    fn files(&self) -> &Files {
        &self.files
    }

    fn python_version(&self) -> PythonVersion {
        Program::get(self).python_version(self)
    }
}

impl Upcast<dyn ruff_db::Db> for CorpusDb {
    fn upcast(&self) -> &(dyn ruff_db::Db + 'static) {
        self
    }
    fn upcast_mut(&mut self) -> &mut (dyn ruff_db::Db + 'static) {
        self
    }
}

#[salsa::db]
impl ty_python_semantic::Db for CorpusDb {
    fn is_file_open(&self, file: File) -> bool {
        !file.path(self).is_vendored_path()
    }

    fn rule_selection(&self) -> &RuleSelection {
        &self.rule_selection
    }

    fn lint_registry(&self) -> &LintRegistry {
        default_lint_registry()
    }
}

#[salsa::db]
impl salsa::Database for CorpusDb {}
