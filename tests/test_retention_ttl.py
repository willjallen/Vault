import asyncio
import datetime as dt
import unittest

from tests.support import (
    FAKE_REQUEST,
    add_permission,
    auth_headers,
    create_versioned_document,
    user_context,
    vault_runtime,
    vault_test_client,
)

from app.models import Document, DocumentLock, Folder, StateEvent, VaultGroup
from app.routers import (
    apply_folder_ttl,
    checkin_document,
    folder_path,
    get_or_create_folder_path,
    get_root_folder,
    move_doc_item,
    move_folder_item,
    normalize_timestamp,
    now_utc,
    restore_doc_item,
    sweep_expired_documents,
)


class RetentionTtlTests(unittest.TestCase):
    def test_expired_document_is_archived_to_flat_archive_with_origin_metadata(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Project")
                folder.default_ttl_days = 30
                folder.default_ttl_action = "archive"
                doc = Document(folder_id=folder.id, name="plan.txt", latest_modified_at=now_utc())
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=31))
                self.assertLessEqual(doc.expires_at, now_utc())
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["archived"], ["Archive/plan.txt"])
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(folder_path(doc.folder), "Archive")
                self.assertEqual(doc.archived_from_folder, "Project")
                self.assertEqual(doc.archived_original_name, "plan.txt")
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)
                event = db.query(StateEvent).filter_by(event_type="retention.expired").one()
                self.assertEqual(
                    event.payload["resources"],
                    ["contents", "document_detail", "my_edits", "sidebar"],
                )

                restore_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(folder_path(doc.folder), "Project")
                self.assertEqual(doc.expiry_action, "archive")
                self.assertIsNotNone(doc.expires_at)
                threshold = now_utc() + dt.timedelta(days=29)
                self.assertGreater(normalize_timestamp(doc.expires_at), threshold)

    def test_expired_document_can_be_deleted_without_archive_first(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Temp")
                folder.default_ttl_days = 1
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="scratch.txt",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=2))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["archived"], [])
            self.assertEqual(result["deleted"], ["Temp/scratch.txt"])

            with ctx.db() as db:
                self.assertIsNone(db.get(Document, doc_id))

    def test_locked_expired_document_is_skipped(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Working")
                folder.default_ttl_days = 1
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="locked.txt",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, now_utc() - dt.timedelta(days=2))
                db.add(DocumentLock(document_id=doc.id, locked_by="user", is_active=True))
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["skipped"], ["Working/locked.txt"])
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNotNone(doc.expires_at)
                self.assertEqual(doc.expiry_action, "delete")

    def test_plain_folders_do_not_compute_delete_ttl_for_old_documents(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Safe")
                doc = Document(
                    folder_id=folder.id,
                    name="old-but-safe.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=365),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result, {"archived": [], "deleted": [], "skipped": []})

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)
                event_count = db.query(StateEvent).filter_by(event_type="retention.expired").count()
                self.assertEqual(event_count, 0)

    def test_child_folder_inherits_parent_delete_ttl(self) -> None:
        with vault_runtime() as ctx:
            with ctx.db() as db:
                parent = get_or_create_folder_path(db, "Temp")
                parent.default_ttl_days = 1
                parent.default_ttl_action = "delete"
                child = get_or_create_folder_path(db, "Temp/Keep")
                doc = Document(
                    folder_id=child.id,
                    name="child-safe.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=30),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, child, now_utc() - dt.timedelta(days=30))
                self.assertEqual(doc.expiry_action, "delete")
                self.assertLessEqual(normalize_timestamp(doc.expires_at), now_utc())
                safe = get_or_create_folder_path(db, "Safe")
                safe_doc = Document(
                    folder_id=safe.id,
                    name="old-but-outside-scope.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=30),
                )
                db.add(safe_doc)
                db.flush()
                apply_folder_ttl(safe_doc, safe, safe_doc.latest_modified_at)
                self.assertIsNone(safe_doc.expiry_action)
                self.assertIsNone(safe_doc.expires_at)
                db.commit()
                doc_id = doc.id
                safe_doc_id = safe_doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], ["Temp/Keep/child-safe.txt"])

            with ctx.db() as db:
                self.assertIsNone(db.get(Document, doc_id))
                self.assertIsNotNone(db.get(Document, safe_doc_id))

    def test_retention_update_reapplies_existing_subtree_and_contents_payload(self) -> None:
        headers = auth_headers("alice", ["vault-admin"])
        with vault_test_client() as ctx:
            with ctx.db() as db:
                get_or_create_folder_path(db, "Project")
                child = get_or_create_folder_path(db, "Project/Concept")
                doc = Document(
                    folder_id=child.id,
                    name="sketch.png",
                    latest_modified_at=now_utc() - dt.timedelta(days=5),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id

            update = ctx.client.put(
                "/api/folders/retention",
                json={
                    "path": "Project",
                    "default_ttl_action": "archive",
                    "default_ttl_days": 30,
                },
                headers=headers,
            )
            self.assertEqual(update.status_code, 200, update.text)
            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.expiry_action, "archive")
                self.assertIsNotNone(doc.expires_at)
                self.assertGreater(
                    normalize_timestamp(doc.expires_at),
                    now_utc() + dt.timedelta(days=24),
                )

            contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Project"},
                headers=headers,
            )
            self.assertEqual(contents.status_code, 200, contents.text)
            child_row = contents.json()["folders"][0]
            self.assertEqual(child_row["path"], "Project/Concept")
            self.assertEqual(child_row["default_ttl_action"], "none")
            self.assertIsNone(child_row["default_ttl_days"])
            self.assertEqual(child_row["effective_ttl_action"], "archive")
            self.assertEqual(child_row["effective_ttl_days"], 30)
            self.assertTrue(child_row["effective_ttl_inherited"])

            child_contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Project/Concept"},
                headers=headers,
            )
            self.assertEqual(child_contents.status_code, 200, child_contents.text)
            doc_row = child_contents.json()["documents"][0]
            self.assertEqual(doc_row["expiry_action"], "archive")
            self.assertIsNotNone(doc_row["expires_at"])

            clear = ctx.client.put(
                "/api/folders/retention",
                json={
                    "path": "Project",
                    "default_ttl_action": "none",
                    "default_ttl_days": None,
                },
                headers=headers,
            )
            self.assertEqual(clear.status_code, 200, clear.text)
            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNone(doc.expiry_action)
                self.assertIsNone(doc.expires_at)

    def test_retention_update_rejects_inaccessible_descendants(self) -> None:
        writer_headers = auth_headers("writer", ["writers"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                confidential = VaultGroup(name="confidential")
                db.add_all([writers, confidential])
                db.flush()
                add_permission(db, root, writers, write=True)

                project = get_or_create_folder_path(db, "Project")
                private = get_or_create_folder_path(db, "Project/Private")
                db.flush()
                add_permission(db, project, writers, write=True)
                add_permission(db, private, confidential, write=True)

                secret = create_versioned_document(
                    db,
                    private,
                    name="secret.txt",
                    data=b"secret",
                )
                project_id = project.id
                secret_id = secret.id
                db.commit()

            update = ctx.client.put(
                "/api/folders/retention",
                json={
                    "path": "Project",
                    "default_ttl_action": "archive",
                    "default_ttl_days": 30,
                },
                headers=writer_headers,
            )
            self.assertEqual(update.status_code, 404, update.text)

            with ctx.db() as db:
                project = db.get(Folder, project_id)
                secret = db.get(Document, secret_id)
                self.assertIsNone(project.default_ttl_action)
                self.assertIsNone(project.default_ttl_days)
                self.assertIsNone(secret.expiry_action)
                self.assertIsNone(secret.expires_at)

    def test_moving_folder_out_of_ttl_scope_recalculates_descendant_documents(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                expiring = get_or_create_folder_path(db, "Expiring")
                expiring.default_ttl_days = 1
                expiring.default_ttl_action = "delete"
                child = get_or_create_folder_path(db, "Expiring/Work")
                get_or_create_folder_path(db, "Safe")
                doc = Document(
                    folder_id=child.id,
                    name="asset.fbx",
                    latest_modified_at=now_utc() - dt.timedelta(days=10),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, child, doc.latest_modified_at)
                self.assertEqual(doc.expiry_action, "delete")
                move_folder_item(child, "Safe", admin, db)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(folder_path(doc.folder), "Safe/Work")
                self.assertIsNone(doc.expiry_action)
                self.assertIsNone(doc.expires_at)

    def test_moving_from_delete_ttl_folder_to_plain_folder_clears_delete_expiry(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                source = get_or_create_folder_path(db, "Temp")
                source.default_ttl_days = 1
                source.default_ttl_action = "delete"
                get_or_create_folder_path(db, "Safe")
                doc = Document(folder_id=source.id, name="rescue.txt", latest_modified_at=now_utc())
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, source, now_utc() - dt.timedelta(days=2))
                self.assertEqual(doc.expiry_action, "delete")
                move_doc_item(doc, "Safe", FAKE_REQUEST, admin, db)
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertIsNone(doc.expires_at)
                self.assertIsNone(doc.expiry_action)
                self.assertEqual(doc.folder.name, "Safe")

    def test_renaming_in_delete_ttl_folder_refreshes_expiry_from_new_modified_time(self) -> None:
        admin = user_context("alice", groups=["vault-admin"])

        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Temp")
                folder.default_ttl_days = 7
                folder.default_ttl_action = "delete"
                doc = Document(
                    folder_id=folder.id,
                    name="draft.txt",
                    latest_modified_at=now_utc() - dt.timedelta(days=30),
                )
                db.add(doc)
                db.flush()
                apply_folder_ttl(doc, folder, doc.latest_modified_at)
                self.assertLessEqual(normalize_timestamp(doc.expires_at), now_utc())
                db.commit()

                renamed_path = move_doc_item(
                    doc,
                    "Temp",
                    FAKE_REQUEST,
                    admin,
                    db,
                    name="draft-renamed.txt",
                )
                self.assertEqual(renamed_path, "Temp/draft-renamed.txt")
                self.assertGreater(
                    normalize_timestamp(doc.expires_at),
                    now_utc() + dt.timedelta(days=6),
                )
                db.commit()
                doc_id = doc.id

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.name, "draft-renamed.txt")
                self.assertEqual(doc.expiry_action, "delete")

    def test_checkin_refreshes_delete_ttl_from_new_version_time(self) -> None:
        user = user_context("alice", groups=["vault-admin"])

        class Upload:
            filename = "draft.txt"
            content_type = "text/plain"

            def __init__(self) -> None:
                self._sent = False

            async def read(self, size: int = -1) -> bytes:
                del size
                if self._sent:
                    return b""
                self._sent = True
                return b"fresh content"

        with vault_runtime() as ctx:
            with ctx.db() as db:
                folder = get_or_create_folder_path(db, "Temp")
                folder.default_ttl_days = 7
                folder.default_ttl_action = "delete"
                doc = create_versioned_document(
                    db,
                    folder,
                    name="draft.txt",
                    data=b"old content",
                    actor=user,
                    committed_at=now_utc() - dt.timedelta(days=30),
                )
                apply_folder_ttl(doc, folder, doc.latest_modified_at)
                self.assertLessEqual(normalize_timestamp(doc.expires_at), now_utc())
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by=str(user["id"]),
                        locked_by_name=str(user["name"]),
                    ),
                )
                db.commit()
                doc_id = doc.id

            with ctx.db() as db:
                result = asyncio.run(
                    checkin_document(
                        doc_id,
                        FAKE_REQUEST,
                        Upload(),
                        "new version",
                        False,
                        user,
                        db,
                    ),
                )
                self.assertEqual(result["path"], "Temp/draft.txt")
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertGreater(
                    normalize_timestamp(doc.expires_at),
                    now_utc() + dt.timedelta(days=6),
                )

            result = sweep_expired_documents()
            self.assertEqual(result["deleted"], [])

            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(doc)
                self.assertEqual(doc.expiry_action, "delete")


if __name__ == "__main__":
    unittest.main()
