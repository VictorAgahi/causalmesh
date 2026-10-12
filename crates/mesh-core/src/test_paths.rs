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
    "__test-utils__",
    "__test_utils__",
    "__testutils__",
    "__specs__",
    "__e2e__",
    "__testing__",
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

/// Whether `path` looks machine-generated (protobuf/gRPC stubs, codegen output,
/// declaration files). Used only to rank a representative location below
/// hand-written code, never to hide a dependent.
pub fn is_generated_path(path: &Path) -> bool {
    let in_gen_dir = path.components().any(|c| match c {
        Component::Normal(name) => name.to_str().is_some_and(|n| {
            ["__generated__", "generated", "gen", "codegen", "proto-gen"]
                .iter()
                .any(|d| d.eq_ignore_ascii_case(n))
        }),
        _ => false,
    });
    if in_gen_dir {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".d.ts")
        || name.contains(".generated.")
        || name.contains(".gen.")
        || name.contains(".pb.")
        || name.contains("_pb.")
        || name.contains("_pb2")
        || name.contains("_grpc.")
}

/// Whether `node` is declared in test code. The path is judged RELATIVE to its
/// workspace root: a checkout that itself lives under `~/testing/` or `/work/e2e/`
/// must not classify every node as test code. A node under no known root keeps
/// its full path. Every call site that classifies graph nodes uses this.
pub fn is_test_node(node: &crate::types::ContractNode, roots: &[std::path::PathBuf]) -> bool {
    let rel = roots
        .get(node.repo_id as usize)
        .and_then(|r| node.file_path.strip_prefix(r).ok())
        .or_else(|| {
            roots
                .iter()
                .filter_map(|r| node.file_path.strip_prefix(r).ok())
                .min_by_key(|rel| rel.components().count())
        })
        .unwrap_or(&node.file_path);
    is_test_path(rel)
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
            "web/src/__fixtures__/events.ts",
            "libs/common/src/__test-utils__/factory.ts",
            "libs/common/src/__testutils__/factory.ts",
            "src/events/events.controller.test.stories.ts",
            "src/events/events.spec.helpers.ts",
            "src/events/events.e2e-spec.ts",
        ] {
            assert!(is_test_path(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn recognizes_generated_code() {
        for p in [
            "src/__generated__/schema.ts",
            "gen/go/user.pb.go",
            "src/user_pb.js",
            "types/api.d.ts",
            "src/client.generated.ts",
            "proto/user_pb2.py",
        ] {
            assert!(is_generated_path(Path::new(p)), "{p}");
        }
        for p in ["src/user.service.ts", "src/general/index.ts", "src/pbx.ts"] {
            assert!(!is_generated_path(Path::new(p)), "{p}");
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
            "src/events/events.controller.ts",
            "src/__generated__/schema.ts",
            "src/contest.config.ts",
        ] {
            assert!(!is_test_path(Path::new(p)), "{p}");
        }
    }
}
