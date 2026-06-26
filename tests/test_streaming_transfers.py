import asyncio
import io
import unittest
import zipfile

from fastapi import HTTPException
from fastapi.responses import StreamingResponse
from tests.support import (
    FAKE_REQUEST,
    collect_response_body,
    create_versioned_document,
    user_context,
    vault_runtime,
)

import app.routers as routers
from app.models import Blob, BlobLocation, Document
from app.routers import (
    ActionItem,
    ActionPayload,
    create_document,
    download_items,
    get_or_create_folder_path,
)
from app.storage import get_storage_backend


class ChunkedUpload:
    filename = "chunked.txt"
    content_type = "text/plain"

    def __init__(self, chunks: list[bytes]) -> None:
        self._chunks = list(chunks)
        self.read_sizes: list[int] = []

    async def read(self, size: int = -1) -> bytes:
        self.read_sizes.append(size)
        if not self._chunks:
            return b""
        return self._chunks.pop(0)


class FailingUpload:
    filename = "partial.txt"
    content_type = "text/plain"

    def __init__(self) -> None:
        self._sent = False

    async def read(self, size: int = -1) -> bytes:
        del size
        if not self._sent:
            self._sent = True
            return b"partial"
        raise OSError("client disconnected")


class StreamingTransferTests(unittest.TestCase):
    def test_upload_streams_chunks_and_download_streams_file(self) -> None:
        user = user_context("uploader")
        upload = ChunkedUpload([b"hello ", b"world"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                result = asyncio.run(create_document(FAKE_REQUEST, upload, "", user, db))
                doc_id = int(result["id"])

                self.assertGreaterEqual(len(upload.read_sizes), 3)
                self.assertTrue(
                    all(size == routers.STREAM_CHUNK_SIZE for size in upload.read_sizes)
                )

                response = download_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FAKE_REQUEST,
                    user,
                    db,
                )

            self.assertIsInstance(response, StreamingResponse)
            self.assertEqual(collect_response_body(response), b"hello world")

    def test_upload_size_limit_rejects_before_metadata_or_blob_write(self) -> None:
        user = user_context("uploader")

        with vault_runtime() as ctx:
            routers.configure_router_runtime(max_upload_bytes=5)
            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    asyncio.run(
                        create_document(
                            FAKE_REQUEST,
                            ChunkedUpload([b"abc", b"def"]),
                            "",
                            user,
                            db,
                        ),
                    )
                db.rollback()

                self.assertEqual(raised.exception.status_code, 413)
                self.assertEqual(db.query(Document).count(), 0)
                self.assertEqual(db.query(Blob).count(), 0)
                self.assertEqual(db.query(BlobLocation).count(), 0)
                self.assertEqual(get_storage_backend("local").list_object_keys(), [])

    def test_failed_upload_read_rejects_before_metadata_or_blob_write(self) -> None:
        user = user_context("uploader")

        with vault_runtime() as ctx:
            with ctx.db() as db:
                with self.assertRaises(HTTPException) as raised:
                    asyncio.run(create_document(FAKE_REQUEST, FailingUpload(), "", user, db))
                db.rollback()

                self.assertEqual(raised.exception.status_code, 400)
                self.assertEqual(
                    raised.exception.detail,
                    "Upload failed while reading request body",
                )
                self.assertEqual(db.query(Document).count(), 0)
                self.assertEqual(db.query(Blob).count(), 0)
                self.assertEqual(db.query(BlobLocation).count(), 0)
                self.assertEqual(get_storage_backend("local").list_object_keys(), [])

    def test_zip_download_streams_from_temp_file(self) -> None:
        user = user_context("downloader")

        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(
                    db,
                    folder,
                    name="one.txt",
                    data=b"one",
                    actor=user,
                )
                create_versioned_document(
                    db,
                    folder,
                    name="two.txt",
                    data=b"two",
                    actor=user,
                )
                db.commit()
                folder_id = folder.id

            with ctx.db() as db:
                response = download_items(
                    ActionPayload(items=[ActionItem(type="folder", id=folder_id)]),
                    FAKE_REQUEST,
                    user,
                    db,
                )

            self.assertIsInstance(response, StreamingResponse)
            with zipfile.ZipFile(io.BytesIO(collect_response_body(response))) as archive:
                self.assertEqual(archive.read("Project/one.txt"), b"one")
                self.assertEqual(archive.read("Project/two.txt"), b"two")


if __name__ == "__main__":
    unittest.main()
