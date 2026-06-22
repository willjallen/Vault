import asyncio
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class AclPermissionTests(unittest.TestCase):
    def run_script(self, script: str) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-acl-") as temp_dir:
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(Path(temp_dir) / "vault.db")
            env["VAULT_OBJECTS_PATH"] = str(Path(temp_dir) / "objects")

            completed = subprocess.run(
                [sys.executable, "-c", textwrap.dedent(script)],
                check=False,
                cwd=Path(__file__).resolve().parents[1],
                env=env,
                stderr=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)

    def test_folder_acl_enforces_visible_read_and_write_paths(self) -> None:
        self.run_script(
            """
            import asyncio

            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Document, DocumentLock, Folder, FolderPermission, VaultGroup
            from app.routers import (
                ActionItem,
                ActionPayload,
                build_contents_payload,
                create_document,
                create_document_version,
                download_items,
                get_or_create_blob_for_data,
                get_or_create_folder_path,
                get_root_folder,
                now_utc,
                unlock_items,
            )
            from app.storage import ensure_storage


            class FakeClient:
                host = "testclient"


            class FakeRequest:
                headers = {}
                client = FakeClient()


            class Upload:
                filename = "writer.txt"
                content_type = "text/plain"

                async def read(self):
                    return b"writer"


            def user(user_id, groups, is_admin=False):
                return {
                    "id": user_id,
                    "vault_user_id": 0,
                    "issuer": "test",
                    "subject": user_id,
                    "name": user_id.title(),
                    "email": f"{user_id}@example.com",
                    "groups": groups,
                    "is_admin": is_admin,
                }


            def add_permission(db, folder, group, view, read, write):
                db.add(
                    FolderPermission(
                        folder_id=folder.id,
                        group_id=group.id,
                        can_view=view,
                        can_read=read,
                        can_write=write,
                    ),
                )


            def add_doc(db, folder, name, data, actor):
                blob = get_or_create_blob_for_data(db, data, "text/plain")
                doc = Document(
                    folder_id=folder.id,
                    name=name,
                    created_by=actor["id"],
                    created_by_name=actor["name"],
                    latest_modified_by=actor["id"],
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                create_document_version(
                    db,
                    doc,
                    blob,
                    actor,
                    {"ip": None, "user_agent": None},
                    name,
                    "text/plain",
                    f"Uploaded {name}",
                    "upload",
                )
                return doc


            admin = user("admin", ["vault-admin"], True)
            viewer = user("viewer", ["viewers"])
            reader = user("reader", ["readers"])
            writer = user("writer", ["writers"])
            outsider = user("outsider", ["outsiders"])

            init_db()
            ensure_storage()
            with SessionLocal() as db:
                root = get_root_folder(db, "vault")
                viewers = VaultGroup(name="viewers")
                readers = VaultGroup(name="readers")
                writers = VaultGroup(name="writers")
                outsiders = VaultGroup(name="outsiders")
                db.add_all([viewers, readers, writers, outsiders])
                db.flush()

                for group in (viewers, readers, writers, outsiders):
                    add_permission(db, root, group, True, True, False)

                project = get_or_create_folder_path(db, "Project")
                project_id = project.id
                db.commit()

                project = db.get(Folder, project_id)
                db.query(FolderPermission).filter_by(folder_id=project.id).delete()
                db.flush()
                add_permission(db, project, viewers, True, False, False)
                add_permission(db, project, readers, True, True, False)
                add_permission(db, project, writers, True, True, True)
                doc = add_doc(db, project, "plan.txt", b"secret", admin)
                db.add(
                    DocumentLock(
                        document_id=doc.id,
                        locked_by=reader["id"],
                        locked_by_name=reader["name"],
                    ),
                )
                doc_id = doc.id
                db.commit()

            with SessionLocal() as db:
                viewer_root = build_contents_payload(db, "", viewer)
                assert [folder["path"] for folder in viewer_root["folders"]] == ["Project"]

                outsider_root = build_contents_payload(db, "", outsider)
                assert outsider_root["folders"] == []

                viewer_project = build_contents_payload(db, "Project", viewer)
                assert viewer_project["documents"][0]["access"] == {
                    "visible": True,
                    "read": False,
                    "write": False,
                }

                try:
                    download_items(
                        ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                        FakeRequest(),
                        viewer,
                        db,
                    )
                except HTTPException as exc:
                    assert exc.status_code == 403
                else:
                    raise AssertionError("visible-only user downloaded the document")

                response = download_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FakeRequest(),
                    reader,
                    db,
                )
                assert response.body == b"secret"

                result = unlock_items(
                    ActionPayload(items=[ActionItem(type="document", id=doc_id)]),
                    FakeRequest(),
                    writer,
                    db,
                )
                assert result["ok"][0]["detail"] == "Unlocked"
                assert db.query(DocumentLock).filter_by(document_id=doc_id, is_active=True).count() == 0

                try:
                    asyncio.run(create_document(FakeRequest(), Upload(), "Project", reader, db))
                except HTTPException as exc:
                    assert exc.status_code == 403
                else:
                    raise AssertionError("read-only user created a document")
                finally:
                    db.rollback()

                result = asyncio.run(create_document(FakeRequest(), Upload(), "Project", writer, db))
                assert result["path"] == "Project/writer.txt"
            """,
        )

    def test_created_folders_default_to_write_for_existing_groups_and_missing_acl_denies(self) -> None:
        self.run_script(
            """
            from fastapi import HTTPException

            from app.db import SessionLocal, init_db
            from app.models import Folder, FolderPermission, VaultGroup
            from app.routers import build_contents_payload, create_folder, get_root_folder
            from app.storage import ensure_storage


            def user(user_id, groups, is_admin=False):
                return {
                    "id": user_id,
                    "vault_user_id": 0,
                    "issuer": "test",
                    "subject": user_id,
                    "name": user_id.title(),
                    "email": f"{user_id}@example.com",
                    "groups": groups,
                    "is_admin": is_admin,
                }


            admin = user("admin", ["vault-admin"], True)
            writer = user("writer", ["writers"])

            init_db()
            ensure_storage()
            with SessionLocal() as db:
                root = get_root_folder(db, "vault")
                writers = VaultGroup(name="writers")
                db.add(writers)
                db.flush()

                create_folder("Open", admin, db)
                open_folder = db.query(Folder).filter_by(parent_id=root.id, name="Open").one()
                rule = (
                    db.query(FolderPermission)
                    .filter_by(folder_id=open_folder.id, group_id=writers.id)
                    .one()
                )
                assert rule.can_view and rule.can_read and rule.can_write

                locked = Folder(root_key="vault", parent_id=root.id, name="Locked", is_root=False)
                db.add(locked)
                db.commit()

            with SessionLocal() as db:
                try:
                    build_contents_payload(db, "Locked", writer)
                except HTTPException as exc:
                    assert exc.status_code == 404
                else:
                    raise AssertionError("ACL-less folder was treated as open")
            """,
        )


if __name__ == "__main__":
    unittest.main()
