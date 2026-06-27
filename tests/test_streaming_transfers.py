import asyncio
import datetime as dt
import io
import queue
import tempfile
import threading
import unittest
import zipfile
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from tests.support import (
    auth_headers,
    create_versioned_document,
    sha256_hex,
    upload_file_via_session,
    user_context,
    vault_test_client,
    wait_for_export,
)

import app.routers as routers
from app.models import (
    Blob,
    BlobLocation,
    Document,
    ExportArtifact,
    ExportJob,
    UploadPart,
    UploadSession,
)
from app.routers import (
    ExportCancelled,
    create_export_temp_path,
    current_version,
    get_or_create_folder_path,
    recover_interrupted_transfers,
    reserve_upload_completion,
    sweep_expired_transfers,
    upload_session_dir,
    write_version_to_zip,
)
from app.storage import get_storage_backend
from app.transfers.engine import _enqueue_part_buffer


class StreamingTransferTests(unittest.TestCase):
    def test_resumable_upload_completes_and_download_supports_ranges(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"hello world"

        with vault_test_client() as ctx:
            uploaded = upload_file_via_session(
                ctx.client,
                headers=headers,
                filename="chunked.txt",
                data=data,
            )
            self.assertEqual(uploaded.status_code, 200, uploaded.text)
            doc_id = uploaded.json()["id"]

            with ctx.db() as db:
                session = db.query(UploadSession).one()
                self.assertEqual(session.status, "complete")
                self.assertEqual(db.query(UploadPart).count(), 0)

            ranged = ctx.client.get(
                f"/documents/{doc_id}/download",
                headers={**headers, "Range": "bytes=6-10"},
            )
            self.assertEqual(ranged.status_code, 206, ranged.text)
            self.assertEqual(ranged.content, b"world")
            self.assertEqual(ranged.headers["content-range"], "bytes 6-10/11")
            self.assertEqual(ranged.headers["accept-ranges"], "bytes")

            invalid = ctx.client.get(
                f"/documents/{doc_id}/download",
                headers={**headers, "Range": "bytes=99-120"},
            )
            self.assertEqual(invalid.status_code, 416)

    def test_upload_size_limit_rejects_before_metadata_or_blob_write(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])

        with vault_test_client() as ctx:
            routers.configure_router_runtime(max_upload_bytes=5)
            response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "too-large.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": 6,
                },
                headers=headers,
            )
            self.assertEqual(response.status_code, 413)
            with ctx.db() as db:
                self.assertEqual(db.query(Document).count(), 0)
                self.assertEqual(db.query(Blob).count(), 0)
                self.assertEqual(db.query(BlobLocation).count(), 0)
                self.assertEqual(db.query(UploadSession).count(), 0)
                self.assertEqual(get_storage_backend("local").list_object_keys(), [])

    def test_part_checksum_failure_leaves_no_blob_or_document_metadata(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdef"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "partial.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            bad_part = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=b"abcd",
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(b"wrong"),
                    "X-Upload-Size": "4",
                },
            )
            self.assertEqual(bad_part.status_code, 400)
            with ctx.db() as db:
                self.assertEqual(db.query(Document).count(), 0)
                self.assertEqual(db.query(Blob).count(), 0)
                self.assertEqual(db.query(BlobLocation).count(), 0)
                self.assertEqual(db.query(UploadPart).count(), 0)

    def test_upload_part_spools_inside_session_directory_before_replace(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcd"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "same-device.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            with patch(
                "app.transfers.engine.tempfile.NamedTemporaryFile",
                wraps=tempfile.NamedTemporaryFile,
            ) as named_temp_file:
                part_response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/1",
                    content=data,
                    headers={
                        **headers,
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": "0",
                        "X-Upload-Sha256": sha256_hex(data),
                        "X-Upload-Size": str(len(data)),
                    },
                )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            self.assertEqual(
                named_temp_file.call_args.kwargs["dir"], upload_session_dir(session["id"]) / "tmp"
            )

    def test_upload_part_token_avoids_db_part_metadata(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcd"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "token-part.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            self.assertTrue(session["upload_token"])

            part_response = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=data,
                headers={
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(data),
                    "X-Upload-Size": str(len(data)),
                    "X-Upload-Token": session["upload_token"],
                },
            )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            self.assertEqual(part_response.json()["uploaded_bytes"], len(data))
            with ctx.db() as db:
                self.assertEqual(db.query(UploadPart).count(), 0)

    def test_concurrent_upload_parts_use_transfer_store_not_db_rows(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefghijklmnop"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "parallel-parts.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            upload_token = session["upload_token"]

            def put_part(index: int, offset: int) -> int:
                chunk = data[offset : offset + session["chunk_size"]]
                response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/{index}",
                    content=chunk,
                    headers={
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": str(offset),
                        "X-Upload-Sha256": sha256_hex(chunk),
                        "X-Upload-Size": str(len(chunk)),
                        "X-Upload-Token": upload_token,
                    },
                )
                return response.status_code

            offsets = list(enumerate(range(0, len(data), session["chunk_size"]), start=1))
            with ThreadPoolExecutor(max_workers=4) as executor:
                statuses = list(executor.map(lambda args: put_part(*args), offsets))

            self.assertEqual(statuses, [200, 200, 200, 200])
            with ctx.db() as db:
                self.assertEqual(db.query(UploadPart).count(), 0)

            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={"sha256": sha256_hex(data)},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 200, completed.text)
            downloaded = ctx.client.get(
                f"/documents/{completed.json()['id']}/download",
                headers=headers,
            )
            self.assertEqual(downloaded.content, data)

    def test_transfer_buffer_enqueue_yields_instead_of_blocking_executor(self) -> None:
        class NonBlockingOnlyQueue:
            def __init__(self) -> None:
                self.attempts = 0
                self.items: list[bytes | None] = []

            def put_nowait(self, item: bytes | None) -> None:
                self.attempts += 1
                if self.attempts == 1:
                    raise queue.Full
                self.items.append(item)

        async def run_enqueue() -> NonBlockingOnlyQueue:
            loop = asyncio.get_running_loop()
            worker = loop.create_future()
            buffers = NonBlockingOnlyQueue()
            await asyncio.wait_for(
                _enqueue_part_buffer(buffers, worker, b"chunk"),  # ty: ignore[arg-type]
                timeout=1.0,
            )
            return buffers

        buffers = asyncio.run(run_enqueue())
        self.assertEqual(buffers.items, [b"chunk"])
        self.assertEqual(buffers.attempts, 2)

    def test_upload_part_does_not_require_client_checksum_header(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcd"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "no-client-hash.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()

            part_response = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=data,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Size": str(len(data)),
                },
            )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            self.assertEqual(part_response.json()["uploaded_parts"][0]["sha256"], sha256_hex(data))

            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 200, completed.text)
            downloaded = ctx.client.get(
                f"/documents/{completed.json()['id']}/download",
                headers=headers,
            )
            self.assertEqual(downloaded.content, data)

    def test_duplicate_part_upload_is_idempotent_but_conflicting_content_is_rejected(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdef"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "retry.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            part_headers = {
                **headers,
                "Content-Type": "application/octet-stream",
                "X-Upload-Offset": "0",
                "X-Upload-Sha256": sha256_hex(b"abcd"),
                "X-Upload-Size": "4",
            }
            first = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=b"abcd",
                headers=part_headers,
            )
            self.assertEqual(first.status_code, 200, first.text)
            duplicate = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=b"abcd",
                headers=part_headers,
            )
            self.assertEqual(duplicate.status_code, 200, duplicate.text)
            conflict = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=b"wxyz",
                headers={
                    **part_headers,
                    "X-Upload-Sha256": sha256_hex(b"wxyz"),
                },
            )
            self.assertEqual(conflict.status_code, 409)

    def test_upload_session_resume_reports_existing_parts_and_completes_without_final_hash(
        self,
    ) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefgh"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "resume.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            first_part = data[:4]
            uploaded_part = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=first_part,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(first_part),
                    "X-Upload-Size": str(len(first_part)),
                },
            )
            self.assertEqual(uploaded_part.status_code, 200, uploaded_part.text)

            resumed = ctx.client.get(f"/api/uploads/{session['id']}", headers=headers)
            self.assertEqual(resumed.status_code, 200, resumed.text)
            self.assertEqual(resumed.json()["uploaded_bytes"], len(first_part))
            self.assertEqual(
                resumed.json()["uploaded_parts"],
                [
                    {
                        "offset": 0,
                        "part_number": 1,
                        "sha256": sha256_hex(first_part),
                        "size_bytes": len(first_part),
                    }
                ],
            )

            second_part = data[4:]
            uploaded_second_part = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/2",
                content=second_part,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "4",
                    "X-Upload-Sha256": sha256_hex(second_part),
                    "X-Upload-Size": str(len(second_part)),
                },
            )
            self.assertEqual(uploaded_second_part.status_code, 200, uploaded_second_part.text)

            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 200, completed.text)
            downloaded = ctx.client.get(
                f"/documents/{completed.json()['id']}/download",
                headers=headers,
            )
            self.assertEqual(downloaded.status_code, 200, downloaded.text)
            self.assertEqual(downloaded.content, data)

    def test_upload_completion_finalizes_parts_without_assembled_temp_file(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefgh"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "direct-finalize.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            for index, offset in enumerate(range(0, len(data), 4), start=1):
                chunk = data[offset : offset + 4]
                part_response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/{index}",
                    content=chunk,
                    headers={
                        **headers,
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": str(offset),
                        "X-Upload-Size": str(len(chunk)),
                    },
                )
                self.assertEqual(part_response.status_code, 200, part_response.text)

            with patch(
                "app.routers.tempfile.NamedTemporaryFile",
                wraps=tempfile.NamedTemporaryFile,
            ) as named_temp_file:
                completed = ctx.client.post(
                    f"/api/uploads/{session['id']}/complete",
                    json={"sha256": sha256_hex(data)},
                    headers=headers,
                )
            self.assertEqual(completed.status_code, 200, completed.text)
            named_temp_file.assert_not_called()
            with ctx.db() as db:
                self.assertEqual(db.query(Document).count(), 1)
                self.assertEqual(db.query(Blob).count(), 1)
                self.assertEqual(db.query(BlobLocation).count(), 1)

    def test_upload_completion_uses_preassembled_transfer_file(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefgh"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "preassembled.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            for index, offset in enumerate(range(0, len(data), 4), start=1):
                chunk = data[offset : offset + 4]
                part_response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/{index}",
                    content=chunk,
                    headers={
                        **headers,
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": str(offset),
                        "X-Upload-Size": str(len(chunk)),
                    },
                )
                self.assertEqual(part_response.status_code, 200, part_response.text)

            backend = get_storage_backend("local")
            with patch.object(backend, "put_part_files", wraps=backend.put_part_files) as put_parts:
                completed = ctx.client.post(
                    f"/api/uploads/{session['id']}/complete",
                    json={"sha256": sha256_hex(data)},
                    headers=headers,
                )
            self.assertEqual(completed.status_code, 200, completed.text)
            put_parts.assert_not_called()
            downloaded = ctx.client.get(
                f"/documents/{completed.json()['id']}/download",
                headers=headers,
            )
            self.assertEqual(downloaded.content, data)

    def test_upload_completion_records_verification_progress(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefgh"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "verification-progress.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            for index, offset in enumerate(range(0, len(data), 4), start=1):
                chunk = data[offset : offset + 4]
                part_response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/{index}",
                    content=chunk,
                    headers={
                        **headers,
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": str(offset),
                        "X-Upload-Size": str(len(chunk)),
                    },
                )
                self.assertEqual(part_response.status_code, 200, part_response.text)

            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={"sha256": sha256_hex(data)},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 200, completed.text)

            status = ctx.client.get(f"/api/uploads/{session['id']}", headers=headers)
            self.assertEqual(status.status_code, 200, status.text)
            self.assertEqual(
                status.json()["verification"],
                {"processed_bytes": len(data), "total_bytes": len(data)},
            )
            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                self.assertEqual(upload_session.verification_total_bytes, len(data))
                self.assertEqual(upload_session.verification_processed_bytes, len(data))

    def test_interrupted_upload_completion_is_recovered_to_active(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefgh"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "recover-completing.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            for index, offset in enumerate(range(0, len(data), 4), start=1):
                chunk = data[offset : offset + 4]
                part_response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/{index}",
                    content=chunk,
                    headers={
                        **headers,
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": str(offset),
                        "X-Upload-Size": str(len(chunk)),
                    },
                )
                self.assertEqual(part_response.status_code, 200, part_response.text)

            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                reserve_upload_completion(upload_session, db)

            recovered = recover_interrupted_transfers(
                enqueue_exports=False,
                cleanup_export_temps=False,
            )

            self.assertEqual(recovered["resumed_uploads"], [session["id"]])
            status = ctx.client.get(f"/api/uploads/{session['id']}", headers=headers)
            self.assertEqual(status.status_code, 200, status.text)
            self.assertEqual(status.json()["status"], "active")
            self.assertIsNone(status.json()["verification"])

            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={"sha256": sha256_hex(data)},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 200, completed.text)
            downloaded = ctx.client.get(
                f"/documents/{completed.json()['id']}/download",
                headers=headers,
            )
            self.assertEqual(downloaded.content, data)

    def test_interrupted_upload_completion_fails_when_parts_are_missing(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdefgh"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "missing-part.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            for index, offset in enumerate(range(0, len(data), 4), start=1):
                chunk = data[offset : offset + 4]
                part_response = ctx.client.put(
                    f"/api/uploads/{session['id']}/parts/{index}",
                    content=chunk,
                    headers={
                        **headers,
                        "Content-Type": "application/octet-stream",
                        "X-Upload-Offset": str(offset),
                        "X-Upload-Size": str(len(chunk)),
                    },
                )
                self.assertEqual(part_response.status_code, 200, part_response.text)

            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                reserve_upload_completion(upload_session, db)
            upload_part_path = upload_session_dir(session["id"]) / "parts" / "00000001.part"
            upload_part_path.unlink()

            recovered = recover_interrupted_transfers(
                enqueue_exports=False,
                cleanup_export_temps=False,
            )

            self.assertEqual(recovered["failed_uploads"], [session["id"]])
            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={"sha256": sha256_hex(data)},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 409)
            self.assertEqual(completed.json()["detail"], "Upload session is failed")
            self.assertFalse(upload_session_dir(session["id"]).exists())

    def test_upload_abort_cleans_parts_and_blocks_completion(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdef"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "cancelled.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            first_part = data[:4]
            part_response = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=first_part,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(first_part),
                    "X-Upload-Size": str(len(first_part)),
                },
            )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            self.assertTrue(upload_session_dir(session["id"]).exists())

            aborted = ctx.client.delete(f"/api/uploads/{session['id']}", headers=headers)
            self.assertEqual(aborted.status_code, 200, aborted.text)
            self.assertEqual(aborted.json()["status"], "aborted")
            self.assertEqual(aborted.json()["uploaded_bytes"], 0)
            self.assertEqual(aborted.json()["uploaded_parts"], [])
            self.assertFalse(upload_session_dir(session["id"]).exists())

            completed = ctx.client.post(
                f"/api/uploads/{session['id']}/complete",
                json={},
                headers=headers,
            )
            self.assertEqual(completed.status_code, 409)
            self.assertEqual(completed.json()["detail"], "Upload session is aborted")
            with ctx.db() as db:
                self.assertEqual(db.query(Document).count(), 0)
                self.assertEqual(db.query(Blob).count(), 0)
                self.assertEqual(db.query(BlobLocation).count(), 0)
                self.assertEqual(db.query(UploadPart).count(), 0)

    def test_upload_abort_requires_owner_or_admin(self) -> None:
        owner_headers = auth_headers("owner", ["vault-admin"])
        intruder_headers = auth_headers("intruder", [])

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "owned.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": 4,
                },
                headers=owner_headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session_id = session_response.json()["id"]

            blocked = ctx.client.delete(f"/api/uploads/{session_id}", headers=intruder_headers)
            self.assertEqual(blocked.status_code, 404)
            visible_to_owner = ctx.client.get(f"/api/uploads/{session_id}", headers=owner_headers)
            self.assertEqual(visible_to_owner.status_code, 200, visible_to_owner.text)
            self.assertEqual(visible_to_owner.json()["status"], "active")

    def test_expired_upload_session_cleans_parts_and_is_not_resumable(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdef"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "expired.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            first_part = data[:4]
            part_response = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=first_part,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(first_part),
                    "X-Upload-Size": str(len(first_part)),
                },
            )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                upload_session.expires_at = routers.now_utc() - dt.timedelta(seconds=1)
                db.commit()

            expired = ctx.client.get(f"/api/uploads/{session['id']}", headers=headers)
            self.assertEqual(expired.status_code, 410)
            self.assertEqual(expired.json()["detail"], "Upload session expired")
            self.assertFalse(upload_session_dir(session["id"]).exists())
            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                self.assertEqual(upload_session.status, "expired")
                self.assertEqual(db.query(UploadPart).count(), 0)

    def test_transfer_sweeper_cleans_abandoned_upload_parts(self) -> None:
        headers = auth_headers("uploader", ["vault-admin"])
        data = b"abcdef"

        with vault_test_client() as ctx:
            session_response = ctx.client.post(
                "/api/uploads",
                json={
                    "filename": "abandoned.txt",
                    "folder": "",
                    "mime_type": "text/plain",
                    "mode": "create",
                    "size_bytes": len(data),
                },
                headers=headers,
            )
            self.assertEqual(session_response.status_code, 200, session_response.text)
            session = session_response.json()
            first_part = data[:4]
            part_response = ctx.client.put(
                f"/api/uploads/{session['id']}/parts/1",
                content=first_part,
                headers={
                    **headers,
                    "Content-Type": "application/octet-stream",
                    "X-Upload-Offset": "0",
                    "X-Upload-Sha256": sha256_hex(first_part),
                    "X-Upload-Size": str(len(first_part)),
                },
            )
            self.assertEqual(part_response.status_code, 200, part_response.text)
            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                upload_session.expires_at = routers.now_utc() - dt.timedelta(seconds=1)
                db.commit()
            self.assertTrue(upload_session_dir(session["id"]).exists())

            expired = sweep_expired_transfers()

            self.assertEqual(expired["expired_uploads"], [session["id"]])
            self.assertFalse(upload_session_dir(session["id"]).exists())
            with ctx.db() as db:
                upload_session = db.get(UploadSession, session["id"])
                self.assertIsNotNone(upload_session)
                self.assertEqual(upload_session.status, "expired")
                self.assertEqual(db.query(UploadPart).count(), 0)

            deleted = sweep_expired_transfers()

            self.assertEqual(deleted["deleted_uploads"], [session["id"]])
            with ctx.db() as db:
                self.assertIsNone(db.get(UploadSession, session["id"]))

    def test_expired_export_artifact_is_not_downloadable_and_is_swept(self) -> None:
        user = user_context("downloader")
        headers = auth_headers("downloader", ["vault-admin"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(db, folder, name="one.txt", data=b"one", actor=user)
                create_versioned_document(db, folder, name="two.txt", data=b"two", actor=user)
                db.commit()
                folder_id = folder.id

            export_response = ctx.client.post(
                "/api/exports",
                json={"items": [{"type": "folder", "id": folder_id}]},
                headers=headers,
            )
            self.assertEqual(export_response.status_code, 200, export_response.text)
            export = wait_for_export(ctx.client, export_response.json()["id"], headers=headers)
            self.assertEqual(export["status"], "complete")
            with ctx.db() as db:
                self.assertEqual(db.query(ExportArtifact).count(), 1)
                artifact = db.query(ExportArtifact).one()
                artifact_blob_id = artifact.blob_id
                artifact_object_keys = [location.object_key for location in artifact.blob.locations]
                self.assertTrue(artifact_object_keys)

            with ctx.db() as db:
                job = db.get(ExportJob, export["id"])
                self.assertIsNotNone(job)
                job.expires_at = routers.now_utc() - dt.timedelta(seconds=1)
                for artifact in job.artifacts:
                    artifact.expires_at = job.expires_at
                db.commit()
            expired_download = ctx.client.get(str(export["download_url"]), headers=headers)
            self.assertEqual(expired_download.status_code, 410)
            self.assertEqual(expired_download.json()["detail"], "Export expired")

            swept = sweep_expired_transfers()
            self.assertEqual(swept["deleted_exports"], [export["id"]])
            self.assertEqual(swept["deleted_export_objects"], artifact_object_keys)
            with ctx.db() as db:
                self.assertIsNone(db.get(ExportJob, export["id"]))
                self.assertIsNone(db.get(Blob, artifact_blob_id))
                self.assertEqual(db.query(ExportArtifact).count(), 0)
                self.assertEqual(
                    db.query(BlobLocation)
                    .filter(BlobLocation.object_key.in_(artifact_object_keys))
                    .count(),
                    0,
                )
            self.assertFalse(
                set(artifact_object_keys) & set(get_storage_backend("local").list_object_keys()),
            )

    def test_export_zip_write_checks_cancellation_between_chunks(self) -> None:
        user = user_context("downloader")
        data = b"x" * (routers.STREAM_CHUNK_SIZE + 1)

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, folder, name="large.bin", data=data, actor=user)
                db.flush()
                version = current_version(doc, db)
                self.assertIsNotNone(version)
                checks = 0

                def should_cancel() -> bool:
                    nonlocal checks
                    checks += 1
                    return checks > 1

                zip_temp = tempfile.NamedTemporaryFile(suffix=".zip", delete=False)
                zip_path = zip_temp.name
                zip_temp.close()
                try:
                    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as archive:
                        with self.assertRaises(ExportCancelled):
                            write_version_to_zip(
                                archive,
                                "Project/large.bin",
                                version,
                                should_cancel=should_cancel,
                            )
                finally:
                    Path(zip_path).unlink(missing_ok=True)
                self.assertGreater(checks, 1)

    def test_export_zip_writer_forces_zip64_entries(self) -> None:
        data = b"payload"
        blob = SimpleNamespace(
            hash_algo="sha256",
            hash=sha256_hex(data),
            locations=[],
            size_bytes=len(data),
        )
        version = SimpleNamespace(blob=blob)

        class Target(io.BytesIO):
            def __enter__(self) -> "Target":
                return self

            def __exit__(self, *args: object) -> None:
                self.close()

        class Archive:
            force_zip64: bool | None = None

            def open(self, _name: str, _mode: str, *, force_zip64: bool = False) -> Target:
                self.force_zip64 = force_zip64
                return Target()

        class Storage:
            @contextmanager
            def open_reader(self, _object_key: str, _bucket: str):
                yield io.BytesIO(data)

        location = SimpleNamespace(backend="local", bucket="", object_key="payload")
        archive = Archive()
        with (
            patch("app.routers.location_for_blob", return_value=location),
            patch("app.routers.get_storage_backend", return_value=Storage()),
        ):
            size = write_version_to_zip(archive, "payload.bin", version)

        self.assertEqual(size, len(data))
        self.assertTrue(archive.force_zip64)

    def test_interrupted_export_recovery_requeues_job_and_cleans_temp_files(self) -> None:
        user = user_context("downloader")

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(
                    db,
                    folder,
                    name="one.txt",
                    data=b"one",
                    actor=user,
                )
                db.flush()
                job = ExportJob(
                    id="interrupted-export",
                    status="finalizing",
                    created_by=str(user["id"]),
                    created_by_name=str(user["name"]),
                    user_context=user,
                    request_payload={"items": [{"type": "document", "id": doc.id}]},
                    filename="vault-download.zip",
                    total_items=1,
                    processed_items=1,
                    total_bytes=3,
                    processed_bytes=3,
                    error="partial",
                    expires_at=routers.now_utc() + dt.timedelta(hours=1),
                )
                db.add(job)
                db.commit()

            temp_path = create_export_temp_path("interrupted-export")
            self.assertEqual(temp_path.parent, routers.TRANSFERS_PATH / "exports")
            temp_path.write_bytes(b"partial zip")

            with patch("app.routers.start_export_job") as start_export_job:
                recovered = recover_interrupted_transfers()

            self.assertEqual(recovered["requeued_exports"], ["interrupted-export"])
            self.assertEqual(recovered["queued_exports"], ["interrupted-export"])
            start_export_job.assert_called_once_with("interrupted-export")
            self.assertIn(str(temp_path), recovered["deleted_export_temps"])
            self.assertFalse(temp_path.exists())
            with ctx.db() as db:
                job = db.get(ExportJob, "interrupted-export")
                self.assertIsNotNone(job)
                self.assertEqual(job.status, "queued")
                self.assertEqual(job.processed_items, 0)
                self.assertEqual(job.processed_bytes, 0)
                self.assertIsNone(job.error)

    def test_export_job_creates_downloadable_zip_artifact(self) -> None:
        user = user_context("downloader")
        headers = auth_headers("downloader", ["vault-admin"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(db, folder, name="one.txt", data=b"one", actor=user)
                create_versioned_document(db, folder, name="two.txt", data=b"two", actor=user)
                db.commit()
                folder_id = folder.id

            export_response = ctx.client.post(
                "/api/exports",
                json={"items": [{"type": "folder", "id": folder_id}]},
                headers=headers,
            )
            self.assertEqual(export_response.status_code, 200, export_response.text)
            self.assertEqual(export_response.json()["filename"], "Project.zip")
            export = wait_for_export(ctx.client, export_response.json()["id"], headers=headers)
            self.assertEqual(export["status"], "complete")
            self.assertEqual(export["filename"], "Project.zip")
            ranged = ctx.client.get(
                str(export["download_url"]),
                headers={**headers, "Accept-Encoding": "gzip", "Range": "bytes=0-1"},
            )
            self.assertEqual(ranged.status_code, 206)
            self.assertEqual(ranged.headers["content-encoding"], "identity")
            self.assertEqual(ranged.content, b"PK")
            response = ctx.client.get(str(export["download_url"]), headers=headers)
            self.assertEqual(response.status_code, 200, response.text)
            self.assertIn('filename="Project.zip"', response.headers["content-disposition"])
            with zipfile.ZipFile(io.BytesIO(response.content)) as archive:
                self.assertEqual(
                    archive.getinfo("Project/one.txt").compress_type,
                    zipfile.ZIP_STORED,
                )
                self.assertEqual(archive.read("Project/one.txt"), b"one")
                self.assertEqual(archive.read("Project/two.txt"), b"two")

    def test_large_export_threshold_uses_zip_deflate(self) -> None:
        user = user_context("downloader")
        headers = auth_headers("downloader", ["vault-admin"])

        with vault_test_client() as ctx:
            routers.configure_router_runtime(
                export_zip_compression_threshold_bytes=1,
                export_zip_compresslevel=1,
            )
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(
                    db,
                    folder,
                    name="compressible.txt",
                    data=b"compress me\n" * 256,
                    actor=user,
                )
                db.commit()
                folder_id = folder.id

            export_response = ctx.client.post(
                "/api/exports",
                json={"items": [{"type": "folder", "id": folder_id}]},
                headers=headers,
            )
            self.assertEqual(export_response.status_code, 200, export_response.text)
            export = wait_for_export(ctx.client, export_response.json()["id"], headers=headers)
            self.assertEqual(export["status"], "complete")
            response = ctx.client.get(str(export["download_url"]), headers=headers)
            self.assertEqual(response.status_code, 200, response.text)
            with zipfile.ZipFile(io.BytesIO(response.content)) as archive:
                info = archive.getinfo("Project/compressible.txt")
                self.assertEqual(info.compress_type, zipfile.ZIP_DEFLATED)
                self.assertLess(info.compress_size, info.file_size)

    def test_large_export_stores_precompressed_entries(self) -> None:
        user = user_context("downloader")
        headers = auth_headers("downloader", ["vault-admin"])

        with vault_test_client() as ctx:
            routers.configure_router_runtime(export_zip_compression_threshold_bytes=1)
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(
                    db,
                    folder,
                    name="image.png",
                    data=b"\x89PNG\r\n\x1a\n" + (b"already compressed" * 128),
                    actor=user,
                    content_type="image/png",
                )
                db.commit()
                folder_id = folder.id

            export_response = ctx.client.post(
                "/api/exports",
                json={"items": [{"type": "folder", "id": folder_id}]},
                headers=headers,
            )
            self.assertEqual(export_response.status_code, 200, export_response.text)
            export = wait_for_export(ctx.client, export_response.json()["id"], headers=headers)
            self.assertEqual(export["status"], "complete")
            response = ctx.client.get(str(export["download_url"]), headers=headers)
            self.assertEqual(response.status_code, 200, response.text)
            with zipfile.ZipFile(io.BytesIO(response.content)) as archive:
                self.assertEqual(
                    archive.getinfo("Project/image.png").compress_type,
                    zipfile.ZIP_STORED,
                )

    def test_export_reports_finalizing_while_artifact_is_promoted(self) -> None:
        user = user_context("downloader")
        headers = auth_headers("downloader", ["vault-admin"])
        entered_hash = threading.Event()
        release_hash = threading.Event()
        real_hash_file = routers.hash_file

        def blocking_hash_file(path: Path) -> tuple[str, int]:
            entered_hash.set()
            if not release_hash.wait(5):
                raise AssertionError("Timed out waiting to release export hash")
            return real_hash_file(path)

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(db, folder, name="one.txt", data=b"one", actor=user)
                db.commit()
                folder_id = folder.id

            with patch("app.routers.hash_file", side_effect=blocking_hash_file):
                export_response = ctx.client.post(
                    "/api/exports",
                    json={"items": [{"type": "folder", "id": folder_id}]},
                    headers=headers,
                )
                self.assertEqual(export_response.status_code, 200, export_response.text)
                job_id = export_response.json()["id"]
                self.assertTrue(entered_hash.wait(5))
                status_response = ctx.client.get(f"/api/exports/{job_id}", headers=headers)
                self.assertEqual(status_response.status_code, 200, status_response.text)
                self.assertEqual(status_response.json()["status"], "finalizing")
                release_hash.set()
                export = wait_for_export(ctx.client, job_id, headers=headers)
                self.assertEqual(export["status"], "complete")

    def test_export_cancel_requires_owner_and_blocks_artifact_download(self) -> None:
        user = user_context("owner")
        owner_headers = auth_headers("owner", ["vault-admin"])
        intruder_headers = auth_headers("intruder", [])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                create_versioned_document(db, folder, name="one.txt", data=b"one", actor=user)
                db.commit()
                folder_id = folder.id

            with patch("app.routers.start_export_job"):
                export_response = ctx.client.post(
                    "/api/exports",
                    json={"items": [{"type": "folder", "id": folder_id}]},
                    headers=owner_headers,
                )
            self.assertEqual(export_response.status_code, 200, export_response.text)
            job_id = export_response.json()["id"]
            self.assertEqual(export_response.json()["status"], "queued")

            blocked = ctx.client.delete(f"/api/exports/{job_id}", headers=intruder_headers)
            self.assertEqual(blocked.status_code, 404)
            cancelled = ctx.client.delete(f"/api/exports/{job_id}", headers=owner_headers)
            self.assertEqual(cancelled.status_code, 200, cancelled.text)
            self.assertEqual(cancelled.json()["status"], "cancelled")

            download = ctx.client.get(f"/api/exports/{job_id}/download", headers=owner_headers)
            self.assertEqual(download.status_code, 409)
            self.assertEqual(download.json()["detail"], "Export is not complete")
            with ctx.db() as db:
                job = db.get(ExportJob, job_id)
                self.assertIsNotNone(job)
                self.assertEqual(job.status, "cancelled")
                self.assertEqual(db.query(ExportArtifact).count(), 0)


if __name__ == "__main__":
    unittest.main()
