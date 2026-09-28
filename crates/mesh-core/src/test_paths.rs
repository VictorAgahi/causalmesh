//! Recognizes test code by path, so answers about production dependencies
//! (`find_dependents`, `analyze_impact`, `analyze_grpc`) can leave it out by
//! default — and say how much they left out — instead of counting a `*.spec.ts`
//! as a producer or a test utility as a consumer.

use std::path::{Component, Path};

/// Directory names that hold only tests, fixtures or test helpers.
const TEST_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "__mocks__",
    "__fixtures__",
    "spec",
    "specs",
    "e2e",
    "test-utils",
    "test_utils",
    "testutils",
    "testing",
    "fixtures",
];

/// Whether `path` is test code: a file named like a test in any supported
/// language (`*.spec.ts`, `*.test.js`, `*_test.go`, `test_*.py`, `*Test.java`,
/// `*_spec.rb`, …) or any file under a test-only directory (`__tests__/`,
/// `test/`, `e2e/`, `test-utils/`, …).
pub fn is_test_path(path: &Path) -> bool {
    let in_test_dir = path.components().any(|c| match c {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|n| TEST_DIRS.iter().any(|d| d.eq_ignore_ascii_case(n))),
        _ => false,
    });
    if in_test_dir {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let stem = name.split('.').next().unwrap_or(name);
    // `foo.spec.ts`, `foo.test.tsx`, `foo.e2e-spec.ts`, `foo.int-test.js`
    let dotted = name.split('.').skip(1).any(|part| {
        matches!(
            part,
            "spec" | "test" | "e2e-spec" | "e2e" | "int-test" | "it"
        )
    });
    dotted
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.starts_with("test_")
        || ((stem.ends_with("Test") || stem.ends_with("Tests") || stem.ends_with("IT"))
            && stem.len() > 4
            && stem.chars().next().is_some_and(char::is_uppercase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_tests_across_languages() {
        for p in [
            "ms-user/src/user.service.spec.ts",
            "app/Button.test.tsx",
            "api/test/app.e2e-spec.ts",
            "svc/handler_test.go",
            "pkg/test_models.py",
            "pkg/models_test.py",
            "core/src/test/java/a/UserServiceTest.java",
            "core/src/main/java/a/UserServiceIT.java",
            "lib/user_spec.rb",
            "npm-packages/packages/test-utils/src/factory.ts",
            "web/src/__tests__/App.tsx",
            "web/src/__mocks__/api.ts",
        ] {
            assert!(is_test_path(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn leaves_production_code_alone() {
        for p in [
            "ms-user/src/user.service.ts",
            "app/Button.tsx",
            "svc/handler.go",
            "pkg/models.py",
            "core/src/main/java/a/UserService.java",
            "core/src/main/java/a/Test.java",
            "core/src/main/java/a/ABTesting.java",
            "src/latest/contest.ts",
            "src/attestation/verify.ts",
            "src/protest_it.ts",
        ] {
            assert!(!is_test_path(Path::new(p)), "{p}");
        }
    }
}
