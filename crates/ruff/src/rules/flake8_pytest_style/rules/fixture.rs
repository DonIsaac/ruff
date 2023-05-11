use anyhow::Result;
use ruff_text_size::{TextLen, TextRange, TextSize};
use rustpython_parser::ast::{self, Arguments, Expr, ExprKind, Keyword, Stmt, StmtKind};
use std::fmt;

use ruff_diagnostics::{AlwaysAutofixableViolation, Violation};
use ruff_diagnostics::{Diagnostic, Edit, Fix};
use ruff_macros::{derive_message_formats, violation};
use ruff_python_ast::call_path::collect_call_path;
use ruff_python_ast::helpers::collect_arg_names;
use ruff_python_ast::source_code::Locator;
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{helpers, visitor};
use ruff_python_semantic::analyze::visibility::is_abstract;
use ruff_python_semantic::context::Context;

use crate::autofix::actions::remove_argument;
use crate::checkers::ast::Checker;
use crate::registry::{AsRule, Rule};

use super::helpers::{
    get_mark_decorators, is_pytest_fixture, is_pytest_yield_fixture, keyword_is_literal,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Parentheses {
    None,
    Empty,
}

impl fmt::Display for Parentheses {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Parentheses::None => fmt.write_str(""),
            Parentheses::Empty => fmt.write_str("()"),
        }
    }
}

#[violation]
pub struct PytestFixtureIncorrectParenthesesStyle {
    expected: Parentheses,
    actual: Parentheses,
}

impl AlwaysAutofixableViolation for PytestFixtureIncorrectParenthesesStyle {
    #[derive_message_formats]
    fn message(&self) -> String {
        let PytestFixtureIncorrectParenthesesStyle { expected, actual } = self;
        format!("Use `@pytest.fixture{expected}` over `@pytest.fixture{actual}`")
    }

    fn autofix_title(&self) -> String {
        let PytestFixtureIncorrectParenthesesStyle { expected, .. } = self;
        match expected {
            Parentheses::None => "Remove parentheses".to_string(),
            Parentheses::Empty => "Add parentheses".to_string(),
        }
    }
}

#[violation]
pub struct PytestFixturePositionalArgs {
    function: String,
}

impl Violation for PytestFixturePositionalArgs {
    #[derive_message_formats]
    fn message(&self) -> String {
        let PytestFixturePositionalArgs { function } = self;
        format!("Configuration for fixture `{function}` specified via positional args, use kwargs")
    }
}

#[violation]
pub struct PytestExtraneousScopeFunction;

impl AlwaysAutofixableViolation for PytestExtraneousScopeFunction {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("`scope='function'` is implied in `@pytest.fixture()`")
    }

    fn autofix_title(&self) -> String {
        "Remove implied `scope` argument".to_string()
    }
}

#[violation]
pub struct PytestMissingFixtureNameUnderscore {
    function: String,
}

impl Violation for PytestMissingFixtureNameUnderscore {
    #[derive_message_formats]
    fn message(&self) -> String {
        let PytestMissingFixtureNameUnderscore { function } = self;
        format!("Fixture `{function}` does not return anything, add leading underscore")
    }
}

#[violation]
pub struct PytestIncorrectFixtureNameUnderscore {
    function: String,
}

impl Violation for PytestIncorrectFixtureNameUnderscore {
    #[derive_message_formats]
    fn message(&self) -> String {
        let PytestIncorrectFixtureNameUnderscore { function } = self;
        format!("Fixture `{function}` returns a value, remove leading underscore")
    }
}

#[violation]
pub struct PytestFixtureParamWithoutValue {
    name: String,
}

impl Violation for PytestFixtureParamWithoutValue {
    #[derive_message_formats]
    fn message(&self) -> String {
        let PytestFixtureParamWithoutValue { name } = self;
        format!(
            "Fixture `{name}` without value is injected as parameter, use \
             `@pytest.mark.usefixtures` instead"
        )
    }
}

#[violation]
pub struct PytestDeprecatedYieldFixture;

impl Violation for PytestDeprecatedYieldFixture {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("`@pytest.yield_fixture` is deprecated, use `@pytest.fixture`")
    }
}

#[violation]
pub struct PytestFixtureFinalizerCallback;

impl Violation for PytestFixtureFinalizerCallback {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("Use `yield` instead of `request.addfinalizer`")
    }
}

#[violation]
pub struct PytestUselessYieldFixture {
    name: String,
}

impl AlwaysAutofixableViolation for PytestUselessYieldFixture {
    #[derive_message_formats]
    fn message(&self) -> String {
        let PytestUselessYieldFixture { name } = self;
        format!("No teardown in fixture `{name}`, use `return` instead of `yield`")
    }

    fn autofix_title(&self) -> String {
        "Replace `yield` with `return`".to_string()
    }
}

#[violation]
pub struct PytestErroneousUseFixturesOnFixture;

impl AlwaysAutofixableViolation for PytestErroneousUseFixturesOnFixture {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("`pytest.mark.usefixtures` has no effect on fixtures")
    }

    fn autofix_title(&self) -> String {
        "Remove `pytest.mark.usefixtures`".to_string()
    }
}

#[violation]
pub struct PytestUnnecessaryAsyncioMarkOnFixture;

impl AlwaysAutofixableViolation for PytestUnnecessaryAsyncioMarkOnFixture {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("`pytest.mark.asyncio` is unnecessary for fixtures")
    }

    fn autofix_title(&self) -> String {
        "Remove `pytest.mark.asyncio`".to_string()
    }
}

#[derive(Default)]
/// Visitor that skips functions
struct SkipFunctionsVisitor<'a> {
    has_return_with_value: bool,
    has_yield_from: bool,
    yield_statements: Vec<&'a Expr>,
    addfinalizer_call: Option<&'a Expr>,
}

impl<'a, 'b> Visitor<'b> for SkipFunctionsVisitor<'a>
where
    'b: 'a,
{
    fn visit_stmt(&mut self, stmt: &'b Stmt) {
        match &stmt.node {
            StmtKind::Return(ast::StmtReturn { value }) => {
                if value.is_some() {
                    self.has_return_with_value = true;
                }
            }
            StmtKind::FunctionDef(_) | StmtKind::AsyncFunctionDef(_) => {}
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'b Expr) {
        match &expr.node {
            ExprKind::YieldFrom(_) => {
                self.has_yield_from = true;
            }
            ExprKind::Yield(ast::ExprYield { value }) => {
                self.yield_statements.push(expr);
                if value.is_some() {
                    self.has_return_with_value = true;
                }
            }
            ExprKind::Call(ast::ExprCall { func, .. }) => {
                if collect_call_path(func).map_or(false, |call_path| {
                    call_path.as_slice() == ["request", "addfinalizer"]
                }) {
                    self.addfinalizer_call = Some(expr);
                };
                visitor::walk_expr(self, expr);
            }
            _ => {}
        }
    }
}

fn get_fixture_decorator<'a>(context: &Context, decorators: &'a [Expr]) -> Option<&'a Expr> {
    decorators.iter().find(|decorator| {
        is_pytest_fixture(context, decorator) || is_pytest_yield_fixture(context, decorator)
    })
}

fn pytest_fixture_parentheses(
    checker: &mut Checker,
    decorator: &Expr,
    fix: Fix,
    expected: Parentheses,
    actual: Parentheses,
) {
    let mut diagnostic = Diagnostic::new(
        PytestFixtureIncorrectParenthesesStyle { expected, actual },
        decorator.range(),
    );
    if checker.patch(diagnostic.kind.rule()) {
        diagnostic.set_fix(fix);
    }
    checker.diagnostics.push(diagnostic);
}

pub fn fix_extraneous_scope_function(
    locator: &Locator,
    stmt_at: TextSize,
    expr_range: TextRange,
    args: &[Expr],
    keywords: &[Keyword],
) -> Result<Edit> {
    remove_argument(locator, stmt_at, expr_range, args, keywords, false)
}

/// PT001, PT002, PT003
fn check_fixture_decorator(checker: &mut Checker, func_name: &str, decorator: &Expr) {
    match &decorator.node {
        ExprKind::Call(ast::ExprCall {
            func,
            args,
            keywords,
        }) => {
            if checker
                .settings
                .rules
                .enabled(Rule::PytestFixtureIncorrectParenthesesStyle)
                && !checker.settings.flake8_pytest_style.fixture_parentheses
                && args.is_empty()
                && keywords.is_empty()
            {
                #[allow(deprecated)]
                let fix = Fix::unspecified(Edit::deletion(func.end(), decorator.end()));
                pytest_fixture_parentheses(
                    checker,
                    decorator,
                    fix,
                    Parentheses::None,
                    Parentheses::Empty,
                );
            }

            if checker
                .settings
                .rules
                .enabled(Rule::PytestFixturePositionalArgs)
                && !args.is_empty()
            {
                checker.diagnostics.push(Diagnostic::new(
                    PytestFixturePositionalArgs {
                        function: func_name.to_string(),
                    },
                    decorator.range(),
                ));
            }

            if checker
                .settings
                .rules
                .enabled(Rule::PytestExtraneousScopeFunction)
            {
                let scope_keyword = keywords
                    .iter()
                    .find(|kw| kw.node.arg.as_ref().map_or(false, |arg| arg == "scope"));

                if let Some(scope_keyword) = scope_keyword {
                    if keyword_is_literal(scope_keyword, "function") {
                        let mut diagnostic =
                            Diagnostic::new(PytestExtraneousScopeFunction, scope_keyword.range());
                        if checker.patch(diagnostic.kind.rule()) {
                            let expr_range = diagnostic.range();
                            #[allow(deprecated)]
                            diagnostic.try_set_fix_from_edit(|| {
                                fix_extraneous_scope_function(
                                    checker.locator,
                                    decorator.start(),
                                    expr_range,
                                    args,
                                    keywords,
                                )
                            });
                        }
                        checker.diagnostics.push(diagnostic);
                    }
                }
            }
        }
        _ => {
            if checker
                .settings
                .rules
                .enabled(Rule::PytestFixtureIncorrectParenthesesStyle)
                && checker.settings.flake8_pytest_style.fixture_parentheses
            {
                #[allow(deprecated)]
                let fix = Fix::unspecified(Edit::insertion(
                    Parentheses::Empty.to_string(),
                    decorator.end(),
                ));
                pytest_fixture_parentheses(
                    checker,
                    decorator,
                    fix,
                    Parentheses::Empty,
                    Parentheses::None,
                );
            }
        }
    }
}

/// PT004, PT005, PT022
fn check_fixture_returns(checker: &mut Checker, stmt: &Stmt, name: &str, body: &[Stmt]) {
    let mut visitor = SkipFunctionsVisitor::default();

    for stmt in body {
        visitor.visit_stmt(stmt);
    }

    if checker
        .settings
        .rules
        .enabled(Rule::PytestIncorrectFixtureNameUnderscore)
        && visitor.has_return_with_value
        && name.starts_with('_')
    {
        checker.diagnostics.push(Diagnostic::new(
            PytestIncorrectFixtureNameUnderscore {
                function: name.to_string(),
            },
            helpers::identifier_range(stmt, checker.locator),
        ));
    } else if checker
        .settings
        .rules
        .enabled(Rule::PytestMissingFixtureNameUnderscore)
        && !visitor.has_return_with_value
        && !visitor.has_yield_from
        && !name.starts_with('_')
    {
        checker.diagnostics.push(Diagnostic::new(
            PytestMissingFixtureNameUnderscore {
                function: name.to_string(),
            },
            helpers::identifier_range(stmt, checker.locator),
        ));
    }

    if checker
        .settings
        .rules
        .enabled(Rule::PytestUselessYieldFixture)
    {
        if let Some(stmt) = body.last() {
            if let StmtKind::Expr(ast::StmtExpr { value }) = &stmt.node {
                if let ExprKind::Yield(_) = value.node {
                    if visitor.yield_statements.len() == 1 {
                        let mut diagnostic = Diagnostic::new(
                            PytestUselessYieldFixture {
                                name: name.to_string(),
                            },
                            stmt.range(),
                        );
                        if checker.patch(diagnostic.kind.rule()) {
                            #[allow(deprecated)]
                            diagnostic.set_fix(Fix::unspecified(Edit::range_replacement(
                                "return".to_string(),
                                TextRange::at(stmt.start(), "yield".text_len()),
                            )));
                        }
                        checker.diagnostics.push(diagnostic);
                    }
                }
            }
        }
    }
}

/// PT019
fn check_test_function_args(checker: &mut Checker, args: &Arguments) {
    args.args.iter().chain(&args.kwonlyargs).for_each(|arg| {
        let name = &arg.node.arg;
        if name.starts_with('_') {
            checker.diagnostics.push(Diagnostic::new(
                PytestFixtureParamWithoutValue {
                    name: name.to_string(),
                },
                arg.range(),
            ));
        }
    });
}

/// PT020
fn check_fixture_decorator_name(checker: &mut Checker, decorator: &Expr) {
    if is_pytest_yield_fixture(&checker.ctx, decorator) {
        checker.diagnostics.push(Diagnostic::new(
            PytestDeprecatedYieldFixture,
            decorator.range(),
        ));
    }
}

/// PT021
fn check_fixture_addfinalizer(checker: &mut Checker, args: &Arguments, body: &[Stmt]) {
    if !collect_arg_names(args).contains(&"request") {
        return;
    }

    let mut visitor = SkipFunctionsVisitor::default();

    for stmt in body {
        visitor.visit_stmt(stmt);
    }

    if let Some(addfinalizer) = visitor.addfinalizer_call {
        checker.diagnostics.push(Diagnostic::new(
            PytestFixtureFinalizerCallback,
            addfinalizer.range(),
        ));
    }
}

/// PT024, PT025
fn check_fixture_marks(checker: &mut Checker, decorators: &[Expr]) {
    for (expr, call_path) in get_mark_decorators(decorators) {
        let name = call_path.last().expect("Expected a mark name");
        if checker
            .settings
            .rules
            .enabled(Rule::PytestUnnecessaryAsyncioMarkOnFixture)
        {
            if *name == "asyncio" {
                let mut diagnostic =
                    Diagnostic::new(PytestUnnecessaryAsyncioMarkOnFixture, expr.range());
                if checker.patch(diagnostic.kind.rule()) {
                    let range = checker.locator.full_lines_range(expr.range());
                    #[allow(deprecated)]
                    diagnostic.set_fix(Fix::unspecified(Edit::range_deletion(range)));
                }
                checker.diagnostics.push(diagnostic);
            }
        }

        if checker
            .settings
            .rules
            .enabled(Rule::PytestErroneousUseFixturesOnFixture)
        {
            if *name == "usefixtures" {
                let mut diagnostic =
                    Diagnostic::new(PytestErroneousUseFixturesOnFixture, expr.range());
                if checker.patch(diagnostic.kind.rule()) {
                    let line_range = checker.locator.full_lines_range(expr.range());
                    #[allow(deprecated)]
                    diagnostic.set_fix(Fix::unspecified(Edit::range_deletion(line_range)));
                }
                checker.diagnostics.push(diagnostic);
            }
        }
    }
}

pub fn fixture(
    checker: &mut Checker,
    stmt: &Stmt,
    name: &str,
    args: &Arguments,
    decorators: &[Expr],
    body: &[Stmt],
) {
    let decorator = get_fixture_decorator(&checker.ctx, decorators);
    if let Some(decorator) = decorator {
        if checker
            .settings
            .rules
            .enabled(Rule::PytestFixtureIncorrectParenthesesStyle)
            || checker
                .settings
                .rules
                .enabled(Rule::PytestFixturePositionalArgs)
            || checker
                .settings
                .rules
                .enabled(Rule::PytestExtraneousScopeFunction)
        {
            check_fixture_decorator(checker, name, decorator);
        }

        if checker
            .settings
            .rules
            .enabled(Rule::PytestDeprecatedYieldFixture)
            && checker.settings.flake8_pytest_style.fixture_parentheses
        {
            check_fixture_decorator_name(checker, decorator);
        }

        if (checker
            .settings
            .rules
            .enabled(Rule::PytestMissingFixtureNameUnderscore)
            || checker
                .settings
                .rules
                .enabled(Rule::PytestIncorrectFixtureNameUnderscore)
            || checker
                .settings
                .rules
                .enabled(Rule::PytestUselessYieldFixture))
            && !is_abstract(&checker.ctx, decorators)
        {
            check_fixture_returns(checker, stmt, name, body);
        }

        if checker
            .settings
            .rules
            .enabled(Rule::PytestFixtureFinalizerCallback)
        {
            check_fixture_addfinalizer(checker, args, body);
        }

        if checker
            .settings
            .rules
            .enabled(Rule::PytestUnnecessaryAsyncioMarkOnFixture)
            || checker
                .settings
                .rules
                .enabled(Rule::PytestErroneousUseFixturesOnFixture)
        {
            check_fixture_marks(checker, decorators);
        }
    }

    if checker
        .settings
        .rules
        .enabled(Rule::PytestFixtureParamWithoutValue)
        && name.starts_with("test_")
    {
        check_test_function_args(checker, args);
    }
}
