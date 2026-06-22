import unittest

from fastapi import HTTPException

from app.routers import (
    download_response,
    ensure_document_upload_folder,
    ensure_folder_creation_path,
    normalize_folder,
    normalize_item_name,
    sanitize_mime_type,
)


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

    def test_download_filename_uses_ascii_fallback_for_unicode_names(self) -> None:
        response = download_response(b"data", "計画😀.txt", "text/plain")

        disposition = response.headers["content-disposition"]
        self.assertIn('filename="___.txt"', disposition)
        self.assertIn("filename*=UTF-8''%E8%A8%88%E7%94%BB%F0%9F%98%80.txt", disposition)

    def test_download_content_type_rejects_malformed_legacy_mime_type(self) -> None:
        response = download_response(b"data", "report.txt", "text/plain\nX-Bad: y")

        self.assertEqual(response.headers["content-type"], "text/plain; charset=utf-8")
        self.assertNotIn("\n", response.headers["content-type"])

    def test_mime_type_sanitizer_rejects_non_ascii_values(self) -> None:
        self.assertEqual(sanitize_mime_type("text/😀", "file.bin"), "application/octet-stream")
        self.assertEqual(
            sanitize_mime_type("text/plain; charset=utf-8", "file.txt"),
            "text/plain; charset=utf-8",
        )

    def test_document_uploads_reject_archive_paths(self) -> None:
        with self.assertRaises(HTTPException) as raised:
            ensure_document_upload_folder("Archive/manual")

        self.assertEqual(raised.exception.status_code, 400)

    def test_folder_creation_rejects_archive_paths(self) -> None:
        with self.assertRaises(HTTPException) as raised:
            ensure_folder_creation_path("Archive/Project")

        self.assertEqual(raised.exception.status_code, 400)


if __name__ == "__main__":
    unittest.main()
