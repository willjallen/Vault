import unittest

from fastapi import HTTPException

from app.routers import download_response, normalize_folder, normalize_item_name


class NameValidationTests(unittest.TestCase):
    def test_folder_paths_reject_embedded_control_characters(self) -> None:
        with self.assertRaises(HTTPException) as raised:
            normalize_folder("safe/bad\nfolder")

        self.assertEqual(raised.exception.status_code, 400)

    def test_item_names_reject_embedded_control_characters(self) -> None:
        with self.assertRaises(HTTPException) as raised:
            normalize_item_name("bad\nname.txt", "File name")

        self.assertEqual(raised.exception.status_code, 400)

    def test_download_filename_strips_legacy_control_characters(self) -> None:
        response = download_response(b"data", "bad\nname.txt", "text/plain")

        disposition = response.headers["content-disposition"]
        self.assertNotIn("\n", disposition)
        self.assertIn('filename="bad_name.txt"', disposition)


if __name__ == "__main__":
    unittest.main()
