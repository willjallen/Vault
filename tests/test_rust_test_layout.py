import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import check_rust_test_layout


class RustTestLayoutTests(unittest.TestCase):
    def run_check(self, root: Path) -> int:
        with (
            patch.object(check_rust_test_layout, "ROOT", root),
            patch.object(check_rust_test_layout, "CRATES_DIR", root / "crates"),
        ):
            return check_rust_test_layout.main()

    def test_allows_crate_level_tests_directory(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-rust-layout-") as tmp:
            root = Path(tmp)
            test_file = root / "crates" / "vault-server" / "tests" / "auth.rs"
            test_file.parent.mkdir(parents=True)
            test_file.write_text("#[test]\nfn accepts_external_tests() {}\n", encoding="utf-8")

            self.assertEqual(self.run_check(root), 0)

    def test_rejects_inline_source_tests(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-rust-layout-") as tmp:
            root = Path(tmp)
            source_file = root / "crates" / "vault-server" / "src" / "lib.rs"
            source_file.parent.mkdir(parents=True)
            source_file.write_text(
                "#[cfg(test)]\nmod tests {\n#[test]\nfn inline_test() {}\n}\n",
                encoding="utf-8",
            )

            self.assertEqual(self.run_check(root), 1)

    def test_rejects_source_tests_subdirectory(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-rust-layout-") as tmp:
            root = Path(tmp)
            source_test_file = root / "crates" / "vault-server" / "src" / "tests" / "auth.rs"
            source_test_file.parent.mkdir(parents=True)
            source_test_file.write_text("#[test]\nfn still_inline() {}\n", encoding="utf-8")

            self.assertEqual(self.run_check(root), 1)


if __name__ == "__main__":
    unittest.main()
