import unittest

from first_run import role_matches


class RoleMatchingTests(unittest.TestCase):
    def test_button_role_names_are_aliases(self):
        for actual in ("button", "push button"):
            for expected in ("button", "push button"):
                with self.subTest(actual=actual, expected=expected):
                    self.assertTrue(role_matches(actual, expected))
                    self.assertTrue(role_matches(actual, ("entry", expected)))

    def test_other_roles_remain_exact(self):
        for actual in ("toggle button", "radio button", "entry", "heading"):
            with self.subTest(actual=actual):
                self.assertTrue(role_matches(actual, actual))
                self.assertFalse(role_matches(actual, ("button", "push button")))
                self.assertFalse(role_matches("button", actual))
        self.assertFalse(role_matches("entry", ("heading", "toggle button")))


if __name__ == "__main__":
    unittest.main()
