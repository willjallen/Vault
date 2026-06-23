import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class ShareLinkTests(unittest.TestCase):
    def run_script(self, script: str) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-share-links-") as temp_dir:
            env = os.environ.copy()
            env["BASE_DOMAIN"] = "localhost"
            env["VAULT_AUTH_MODE"] = "dev"
            env["VAULT_DEV_AUTH"] = "1"
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

    def test_share_links_resolve_to_current_structural_targets(self) -> None:
        self.run_script(
            """
            from app.db import SessionLocal, init_db
            from app.models import Document, Folder
            from app.models import ShareLink
            from app.routers import (
                SYSTEM_USER,
                ShareLinkPayload,
                build_initial_state,
                create_share_target,
                generate_share_code,
                get_or_create_folder_path,
                now_utc,
                resolved_share_payload,
            )


            init_db()
            with SessionLocal() as db:
                folder = get_or_create_folder_path(db, "Art")
                doc = Document(
                    folder_id=folder.id,
                    name="mesh.blend",
                    created_by="test",
                    created_by_name="Test",
                    latest_modified_by="test",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                doc_target = create_share_target(
                    ShareLinkPayload(target_type="document", document_id=doc.id),
                    SYSTEM_USER,
                    db,
                )
                folder_target = create_share_target(
                    ShareLinkPayload(target_type="folder", path="Art"),
                    SYSTEM_USER,
                    db,
                )
                doc_link = ShareLink(
                    code=generate_share_code(db),
                    target_type=doc_target[0],
                    document_id=doc_target[1],
                    folder_id=doc_target[2],
                )
                db.add(doc_link)
                db.flush()
                folder_link = ShareLink(
                    code=generate_share_code(db),
                    target_type=folder_target[0],
                    document_id=folder_target[1],
                    folder_id=folder_target[2],
                )
                db.add(folder_link)
                db.commit()
                doc_id = doc.id
                folder_id = folder.id
                doc_code = doc_link.code
                folder_code = folder_link.code

            with SessionLocal() as db:
                folder = db.get(Folder, folder_id)
                folder.name = "Concepts"
                db.commit()

            with SessionLocal() as db:
                doc_link = db.query(ShareLink).filter_by(code=doc_code).one()
                folder_link = db.query(ShareLink).filter_by(code=folder_code).one()
                resolved_doc = resolved_share_payload(doc_link, SYSTEM_USER, db)
                assert resolved_doc["target_type"] == "document"
                assert resolved_doc["folder"] == "Concepts"
                assert resolved_doc["document_id"] == doc_id

                resolved_folder = resolved_share_payload(folder_link, SYSTEM_USER, db)
                assert resolved_folder["target_type"] == "folder"
                assert resolved_folder["folder"] == "Concepts"

                state = build_initial_state(SYSTEM_USER, "", db, share_code=doc_code)
                assert state["share_code"] == doc_code
            """,
        )


if __name__ == "__main__":
    unittest.main()
