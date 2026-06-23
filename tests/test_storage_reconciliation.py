import unittest

from tests.support import create_versioned_document, user_context, vault_runtime

from app.models import Blob, BlobLocation, DocumentVersion
from app.routers import get_or_create_folder_path, storage_reconciliation_report
from app.storage import get_storage_backend


class StorageReconciliationTests(unittest.TestCase):
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

            applied = storage_reconciliation_report(db, apply=True)
            self.assertEqual(applied["orphan_blob_ids"], [blob_id])
            self.assertTrue(applied["deleted_local_keys"])
            db.commit()

            after = storage_reconciliation_report(db, apply=False)
            self.assertEqual(after["orphan_blob_ids"], [])
            self.assertEqual(after["unreferenced_local_keys"], [])
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

            applied = storage_reconciliation_report(db, apply=True)
            self.assertEqual(applied["orphan_blob_ids"], [blob_id])
            self.assertEqual(applied["deleted_local_keys"], [])
            db.commit()

            after = storage_reconciliation_report(db, apply=False)
            self.assertEqual(after["orphan_blob_ids"], [blob_id])
            self.assertEqual(after["unreferenced_local_keys"], [])
            self.assertEqual(db.query(Blob).count(), 1)
            location = db.query(BlobLocation).one()
            self.assertEqual(location.blob_id, blob_id)
            self.assertEqual(location.backend, "s3")
            self.assertEqual(location.object_key, "objects/sha256/remote-only")


if __name__ == "__main__":
    unittest.main()
