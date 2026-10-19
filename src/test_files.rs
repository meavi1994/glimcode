//! Conservative path conventions, not test discovery: never read or execute project code.
use std::path::Path;

pub fn is_test_file(path: &Path) -> bool {
    // Git paths use '/', but accepting both separators also supports Windows paths.
    let path = path.to_string_lossy().replace('\\', "/");
    let mut parts = path.rsplit('/');
    let name = parts.next().unwrap_or_default();
    if parts.any(is_test_directory) {
        return true;
    }
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let lower = stem.to_ascii_lowercase();
    match extension.to_ascii_lowercase().as_str() {
        "go" | "dart" | "ex" | "exs" => lower.ends_with("_test"),
        "py" => lower.starts_with("test_") || lower.ends_with("_test") || lower == "conftest",
        "rb" => lower.starts_with("test_") || lower.ends_with("_test") || lower.ends_with("_spec"),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts" | "vue" | "svelte" => {
            // Also covers foo.test.d.ts and browser variants such as foo.spec.browser.ts.
            lower
                .split('.')
                .any(|part| matches!(part, "test" | "spec" | "cy"))
        }
        "java" | "kt" | "kts" | "scala" | "groovy" | "cs" | "fs" | "vb" | "swift" | "php" => {
            camel_test_name(stem)
        }
        "rs" | "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" | "m" | "mm" => {
            matches!(lower.as_str(), "test" | "tests")
                || lower.starts_with("test_")
                || lower.ends_with("_test")
                || lower.ends_with("_tests")
                || camel_test_name(stem)
        }
        _ => false,
    }
}

fn is_test_directory(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "test"
            | "tests"
            | "__tests__"
            | "spec"
            | "specs"
            | "__snapshots__"
            | "testdata"
            | "testfixtures"
            | "androidtest"
            | "commontest"
            | "jvmtest"
            | "jstest"
            | "nativetest"
            | "iostest"
            | "integrationtest"
            | "integrationtests"
            | "integration_test"
            | "integration_tests"
            | "e2e"
    ) || [".tests", ".unittests", ".integrationtests", ".uitests"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        || name.ends_with("Tests") // Swift package/Xcode test targets, e.g. MyAppTests.
}

fn camel_test_name(stem: &str) -> bool {
    // Preserve case boundaries: Contest.java and Latest.cs are production names.
    stem.strip_prefix("Test").is_some_and(|rest| {
        rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
    }) || ["Test", "Tests", "TestCase", "Spec"]
        .iter()
        .any(|suffix| stem.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_and_typescript_variants_use_complete_name_segments() {
        for extension in [
            "js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "vue", "svelte",
        ] {
            for marker in ["test", "spec", "cy"] {
                assert!(is_test_file(Path::new(&format!(
                    "src/widget.{marker}.{extension}"
                ))));
            }
            assert!(is_test_file(Path::new(&format!(
                "__tests__/helper.{extension}"
            ))));
            assert!(!is_test_file(Path::new(&format!(
                "src/contest.{extension}"
            ))));
            assert!(!is_test_file(Path::new(&format!(
                "src/specification.{extension}"
            ))));
        }
    }

    #[test]
    fn recognizes_test_conventions_across_languages_and_support_files() {
        for path in [
            "hack/apidiff-changelog/main_test.go",
            "pkg/test_widget.py",
            "pkg/widget_test.py",
            "conftest.py",
            "lib/widget.test.tsx",
            "lib/widget.spec.mts",
            "lib/widget.cy.js",
            "lib/widget.test.d.ts",
            "lib/widget.spec.browser.ts",
            "test.tsx",
            "__tests__/helper.json",
            "tests/fixtures/input.json",
            "spec/support/helper.rb",
            "lib/widget_spec.rb",
            "lib/test_widget.rb",
            "src/test/resources/input.txt",
            "src/androidTest/Helper.kt",
            "src/commonTest/Helper.kt",
            "src/integrationTest/Helper.java",
            "WidgetTest.java",
            "TestWidget.java",
            "WidgetTests.kt",
            "WidgetSpec.scala",
            "WidgetTestCase.java",
            "WidgetTest.php",
            "App.Tests/Helpers.cs",
            "WidgetTests.cs",
            "WidgetTests.fs",
            "WidgetTests.vb",
            "MyAppTests/Helper.swift",
            "WidgetTests.swift",
            "lib/widget_test.dart",
            "integration_test/helper.dart",
            "lib/widget_test.exs",
            "src/tests.rs",
            "src/widget_tests.rs",
            "src/widget_test.cpp",
            "test_widget.c",
            "WidgetTest.hpp",
            "e2e/login.ts",
            "testdata/output.golden",
            "__snapshots__/widget.snap",
            "src\\tests\\helper.rs",
            "TESTS/helper.txt",
        ] {
            assert!(is_test_file(Path::new(path)), "missed {path}");
        }
    }

    #[test]
    fn keeps_similarly_named_production_files_and_mixed_sources() {
        for path in [
            "hack/apidiff-changelog/main.go",
            "src/lib.rs",
            "src/Contest.java",
            "src/Latest.cs",
            "src/Testing.java",
            "src/Testament.java",
            "src/testament.ts",
            "src/special.ts",
            "src/specification.ts",
            "src/contest.go",
            "src/test.go",
            "src/widget_test.go.bak",
            "docs/testing.md",
            "testing/server.go",
            "contest/main.go",
            "fixtures/production.json",
            "config/test.exs",
            "src/TEST.java",
            "test",
            "tests.md",
        ] {
            assert!(!is_test_file(Path::new(path)), "incorrectly hid {path}");
        }
    }
}
