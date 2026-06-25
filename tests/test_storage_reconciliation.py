import unittest

from fastapi import HTTPException
from tests.support import create_versioned_document, user_context, vault_runtime

from app.models import Blob, BlobLocation, DocumentVersion
from app.routers import (
    current_version,
    get_or_create_folder_path,
    read_version_bytes,
    storage_reconciliation_report,
)
from app.storage import get_storage_backend


class StorageReconciliationTests(unittest.TestCase):
    def test_current_version_rejects_missing_current_version_pointer(self) -> None:
        user = user_context("user", groups=[])

        with vault_runtime() as ctx, ctx.db() as db:
            folder = get_or_create_folder_path(db, "")
            doc = create_versioned_document(
                db,
                folder,
                name="kept.txt",
                data=b"trusted content",
                actor=user,
            )
            db.commit()

            doc.current_version_id = "missing-version"
            db.commit()

            with self.assertRaises(HTTPException) as raised:
                current_version(doc, db)

            self.assertEqual(raised.exception.status_code, 500)
            self.assertEqual(
                raised.exception.detail,
                "Current document version metadata is inconsistent",
            )

    def test_read_version_rejects_corrupt_local_object(self) -> None:
        user = user_context("user", groups=[])
        original = b"trusted content"

        with vault_runtime() as ctx, ctx.db() as db:
            folder = get_or_create_folder_path(db, "")
            doc = create_versioned_document(
                db,
                folder,
                name="kept.txt",
                data=original,
                actor=user,
            )
            db.commit()

            version = db.query(DocumentVersion).filter_by(document_id=doc.id).one()
            location = (
                db.query(BlobLocation)
                .filter_by(blob_id=version.blob_id, backend="local")
                .one()
            )
            object_path = ctx.temp_dir / "objects" / location.object_key
            object_path.write_bytes(b"corrupt content")

            with self.assertRaises(HTTPException) as raised:
                read_version_bytes(version)

            self.assertEqual(raised.exception.status_code, 500)
            self.assertEqual(raised.exception.detail, "Blob content does not match metadata")

    def test_report_flags_missing_referenced_local_object(self) -> None:
        user = user_context("user", groups=[])

        with vault_runtime() as ctx, ctx.db() as db:
            folder = get_or_create_folder_path(db, "")
            doc = create_versioned_document(
                db,
                folder,
                name="kept.txt",
                data=b"still referenced",
                actor=user,
            )
            db.commit()

            version = db.query(DocumentVersion).filter_by(document_id=doc.id).one()
            location = (
                db.query(BlobLocation)
                .filter_by(blob_id=version.blob_id, backend="local")
                .one()
            )
            object_key = location.object_key

            get_storage_backend("local").delete_object(object_key)

            report = storage_reconciliation_report(db, apply=False)

            self.assertEqual(report["orphan_blob_ids"], [])
            self.assertEqual(report["unreferenced_local_keys"], [])
            self.assertEqual(report["missing_local_keys"], [object_key])

    def test_report_flags_corrupt_referenced_local_object(self) -> None:
        user = user_context("user", groups=[])

        with vault_runtime() as ctx, ctx.db() as db:
            folder = get_or_create_folder_path(db, "")
            doc = create_versioned_document(
                db,
                folder,
                name="kept.txt",
                data=b"trusted content",
                actor=user,
            )
            db.commit()

            version = db.query(DocumentVersion).filter_by(document_id=doc.id).one()
            location = (
                db.query(BlobLocation)
                .filter_by(blob_id=version.blob_id, backend="local")
                .one()
            )
            object_key = location.object_key
            object_path = ctx.temp_dir / "objects" / object_key
            object_path.write_bytes(b"corrupt content")

            report = storage_reconciliation_report(db, apply=False)

            self.assertEqual(report["orphan_blob_ids"], [])
            self.assertEqual(report["unreferenced_local_keys"], [])
            self.assertEqual(report["missing_local_keys"], [])
            self.assertEqual(report["corrupt_local_keys"], [object_key])

            applied = storage_reconciliation_report(db, apply=True)

            self.assertEqual(applied["corrupt_local_keys"], [object_key])
            self.assertEqual(applied["deleted_local_keys"], [])
            self.assertEqual(object_path.read_bytes(), b"corrupt content")

    def test_apply_preserves_referenced_local_object_with_missing_location_metadata(
        self,
    ) -> None:
        user = user_context("user", groups=[])
        data = b"referenced content with missing metadata"

        with vault_runtime() as ctx, ctx.db() as db:
            folder = get_or_create_folder_path(db, "")
            doc = create_versioned_document(
                db,
                folder,
                name="kept.txt",
                data=data,
                actor=user,
            )
            db.commit()

            version = db.query(DocumentVersion).filter_by(document_id=doc.id).one()
            location = (
                db.query(BlobLocation)
                .filter_by(blob_id=version.blob_id, backend="local")
                .one()
            )
            object_key = location.object_key
            db.delete(location)
            db.commit()

            applied = storage_reconciliation_report(db, apply=True)
            self.assertEqual(applied["orphan_blob_ids"], [])
            self.assertEqual(applied["unreferenced_local_keys"], [])
            self.assertEqual(applied["missing_local_keys"], [])
            self.assertEqual(applied["missing_local_location_keys"], [object_key])
            self.assertEqual(applied["deleted_local_keys"], [])
            db.commit()

            self.assertEqual(get_storage_backend("local").read_bytes(object_key), data)
            restored_location = (
                db.query(BlobLocation)
                .filter_by(blob_id=version.blob_id, backend="local", object_key=object_key)
                .one()
            )
            self.assertEqual(restored_location.bucket, "")
            after = storage_reconciliation_report(db, apply=False)
            self.assertEqual(after["missing_local_location_keys"], [])

    def test_apply_removes_orphan_blob_metadata_and_local_object(self) -> None:
        user = user_context("user", groups=[])

        with vault_runtime() as ctx, ctx.db() as db:
            folder = get_or_create_folder_path(db, "")
            doc = create_versioned_document(
                db,
                folder,
                name="dead.txt",
                data=b"orphan me",
                actor=user,
            )
            db.commit()

            self.assertTrue(get_storage_backend("local").list_object_keys())

            blob_id = doc.versions[0].blob_id
            db.delete(doc)
            db.commit()

            before = storage_reconciliation_report(db, apply=False)
            self.assertEqual(before["orphan_blob_ids"], [blob_id])
            self.assertEqual(before["unreferenced_local_keys"], [])
            self.assertEqual(before["missing_local_keys"], [])

            applied = storage_reconciliation_report(db, apply=True)
            self.assertEqual(applied["orphan_blob_ids"], [blob_id])
            self.assertEqual(applied["missing_local_keys"], [])
            self.assertTrue(applied["deleted_local_keys"])
            db.commit()

            after = storage_reconciliation_report(db, apply=False)
            self.assertEqual(after["orphan_blob_ids"], [])
            self.assertEqual(after["unreferenced_local_keys"], [])
            self.assertEqual(after["missing_local_keys"], [])
            self.assertEqual(db.query(Blob).count(), 0)
            self.assertEqual(db.query(BlobLocation).count(), 0)
            self.assertEqual(db.query(DocumentVersion).count(), 0)
            self.assertEqual(get_storage_backend("local").list_object_keys(), [])

    def test_apply_preserves_remote_orphan_metadata_without_supported_delete(self) -> None:
        with vault_runtime() as ctx, ctx.db() as db:
            blob = Blob(hash_algo="sha256", hash="remote-only", size_bytes=12)
            db.add(blob)
            db.flush()
            db.add(
                BlobLocation(
                    blob_id=blob.id,
                    backend="s3",
                    bucket="vault-prod",
                    object_key="objects/sha256/remote-only",
                ),
            )
            db.commit()
            blob_id = blob.id

            before = storage_reconciliation_report(db, apply=False)
            self.assertEqual(before["orphan_blob_ids"], [blob_id])
            self.assertEqual(before["unreferenced_local_keys"], [])
            self.assertEqual(before["missing_local_keys"], [])

            applied = storage_reconciliation_report(db, apply=True)
            self.assertEqual(applied["orphan_blob_ids"], [blob_id])
            self.assertEqual(applied["missing_local_keys"], [])
            self.assertEqual(applied["deleted_local_keys"], [])
            db.commit()

            after = storage_reconciliation_report(db, apply=False)
            self.assertEqual(after["orphan_blob_ids"], [blob_id])
            self.assertEqual(after["unreferenced_local_keys"], [])
            self.assertEqual(after["missing_local_keys"], [])
            self.assertEqual(db.query(Blob).count(), 1)
            location = db.query(BlobLocation).one()
            self.assertEqual(location.blob_id, blob_id)
            self.assertEqual(location.backend, "s3")
            self.assertEqual(location.object_key, "objects/sha256/remote-only")


if __name__ == "__main__":
    unittest.main()
