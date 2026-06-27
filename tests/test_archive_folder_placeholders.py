import unittest

from tests.support import (
    FAKE_REQUEST,
    auth_headers,
    create_versioned_document,
    user_context,
    vault_runtime,
    vault_test_client,
)

from app.models import Document
from app.routers import (
    archive_doc_item,
    archive_folder_item,
    build_contents_payload,
    document_path,
    folder_path,
    get_folder_by_path,
    get_or_create_folder_path,
    get_root_folder,
    restore_doc_item,
)


class FlatArchiveTests(unittest.TestCase):
    def test_archiving_document_moves_to_archive_root_with_origin_metadata(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_runtime() as ctx, ctx.db() as db:
            source = get_or_create_folder_path(db, "Project/Sub")
            doc = create_versioned_document(db, source, name="plan.txt", actor=admin)

            result = archive_doc_item(doc, FAKE_REQUEST, admin, db)
            db.commit()

            self.assertEqual(result, "Archive/plan.txt")
            self.assertEqual(folder_path(doc.folder), "Archive")
            self.assertEqual(doc.archived_from_folder, "Project/Sub")
            self.assertEqual(doc.archived_original_name, "plan.txt")
            self.assertEqual(doc.archived_access, {})

    def test_archiving_folder_flattens_documents_and_removes_source_tree(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_runtime() as ctx, ctx.db() as db:
            project = get_or_create_folder_path(db, "Project")
            sub = get_or_create_folder_path(db, "Project/Sub")
            root_doc = create_versioned_document(db, project, name="root.txt", actor=admin)
            sub_doc = create_versioned_document(db, sub, name="nested.txt", actor=admin)

            result = archive_folder_item(project, FAKE_REQUEST, admin, db)
            db.commit()

            self.assertEqual(result, "Archive")
            self.assertIsNone(get_folder_by_path(db, "Project"))
            self.assertIsNone(get_folder_by_path(db, "Archive/Project"))
            self.assertEqual(folder_path(root_doc.folder), "Archive")
            self.assertEqual(folder_path(sub_doc.folder), "Archive")
            self.assertEqual(root_doc.archived_from_folder, "Project")
            self.assertEqual(sub_doc.archived_from_folder, "Project/Sub")

    def test_archive_allows_duplicate_display_names_from_different_folders(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_runtime() as ctx, ctx.db() as db:
            one = get_or_create_folder_path(db, "One")
            two = get_or_create_folder_path(db, "Two")
            first = create_versioned_document(db, one, name="plan.txt", actor=admin)
            second = create_versioned_document(db, two, name="plan.txt", actor=admin)

            archive_doc_item(first, FAKE_REQUEST, admin, db)
            archive_doc_item(second, FAKE_REQUEST, admin, db)
            db.commit()

            archive_root = get_root_folder(db, "archive")
            rows = db.query(Document).filter_by(folder_id=archive_root.id, name="plan.txt").all()
            self.assertEqual({row.id for row in rows}, {first.id, second.id})
            self.assertEqual({row.archived_from_folder for row in rows}, {"One", "Two"})

    def test_restore_document_uses_original_location_metadata(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_runtime() as ctx, ctx.db() as db:
            source = get_or_create_folder_path(db, "Project/Sub")
            doc = create_versioned_document(db, source, name="restore.txt", actor=admin)
            archive_doc_item(doc, FAKE_REQUEST, admin, db)
            db.delete(source)
            db.commit()

            result = restore_doc_item(doc, FAKE_REQUEST, admin, db)
            db.commit()

            self.assertEqual(result, "Project/Sub/restore.txt")
            self.assertEqual(document_path(doc), "Project/Sub/restore.txt")
            self.assertIsNone(doc.archived_from_folder)
            self.assertIsNone(doc.archived_original_name)
            self.assertIsNone(doc.archived_access)

    def test_archive_contents_returns_files_without_folders(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)

        with vault_runtime() as ctx, ctx.db() as db:
            source = get_or_create_folder_path(db, "Project/Sub")
            doc = create_versioned_document(db, source, name="plan.txt", actor=admin)
            archive_doc_item(doc, FAKE_REQUEST, admin, db)
            db.commit()

            payload = build_contents_payload(db, "Archive", admin)

            self.assertEqual(payload["folders"], [])
            self.assertEqual([row["id"] for row in payload["documents"]], [doc.id])
            self.assertEqual(payload["documents"][0]["archived_from_folder"], "Project/Sub")
            self.assertEqual(
                payload["documents"][0]["archived_original_path"], "Project/Sub/plan.txt"
            )

    def test_rename_api_rejects_archived_document(self) -> None:
        admin = user_context("admin", groups=["vault-admin"], is_admin=True)
        admin_headers = auth_headers("admin", ["vault-admin"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                source = get_or_create_folder_path(db, "Project")
                doc = create_versioned_document(db, source, name="locked.txt", actor=admin)
                archive_doc_item(doc, FAKE_REQUEST, admin, db)
                db.commit()
                doc_id = doc.id

            renamed = ctx.client.post(
                "/api/rename",
                headers=admin_headers,
                json={
                    "items": [{"type": "document", "id": doc_id}],
                    "name": "renamed.txt",
                },
            )

            self.assertEqual(renamed.status_code, 200, renamed.text)
            self.assertEqual(renamed.json()["ok"], [])
            self.assertEqual(
                renamed.json()["failed"][0]["detail"],
                "Restore archived files before renaming",
            )
            with ctx.db() as db:
                doc = db.get(Document, doc_id)
                self.assertEqual(doc.name, "locked.txt")
                self.assertEqual(document_path(doc), "Archive/locked.txt")


if __name__ == "__main__":
    unittest.main()
