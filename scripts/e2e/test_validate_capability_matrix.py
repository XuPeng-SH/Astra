import unittest

from scripts.e2e.validate_capability_matrix import (
    collect_rust_functions_from_source,
    collect_system_tests_from_source,
)


class CapabilityMatrixValidatorTests(unittest.TestCase):
    def test_system_test_references_ignore_comments_and_literals(self) -> None:
        source = r'''
            // system_test: "commented_out"
            const TEXT: &str = "system_test: \\\"string_value\\\"";
            system_test: "real_test"
        '''

        self.assertEqual(
            collect_system_tests_from_source(source),
            ["real_test"],
        )

    def test_functions_include_macro_generated_tests_and_ignore_fake_text(self) -> None:
        source = r'''
            // matrix_test! { commented_out, 2, "reason", runner }
            /* nested comment
               /* matrix_test! { nested_comment, 2, "reason", runner } */
            */
            const RAW: &str = r#"matrix_test! { raw_string, 2, "reason", runner }"#;
            const TEXT: &str = "fn fake_function() {} matrix_test! { fake_macro, }";

            matrix_test! {
                // comments before the generated name are valid
                #[cfg(any(feature = "a", target_os = "linux"))]
                brace_test, 2, "reason", runner::run
            }
            current_thread_matrix_test![
                #[cfg(test)]
                square_test, "reason", runner::run
            ];
            matrix_test!(paren_test, 1, "reason", runner::run);
            fn literal_test() {}
            not_matrix_test! { must_not_match, 1, "reason", runner::run }
        '''

        functions = collect_rust_functions_from_source(source)
        self.assertTrue({"brace_test", "square_test", "paren_test", "literal_test"} <= functions)
        self.assertNotIn("commented_out", functions)
        self.assertNotIn("fake_function", functions)
        self.assertNotIn("fake_macro", functions)
        self.assertNotIn("must_not_match", functions)


if __name__ == "__main__":
    unittest.main()
