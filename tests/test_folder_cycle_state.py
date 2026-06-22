import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class FolderCycleStateTests(unittest.TestCase):
    def test_folder_path_helpers_tolerate_corrupt_parent_cycle(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-folder-cycle-") as temp_dir:
            script = textwrap.dedent(
                """
                import signal

                from app.db import SessionLocal, init_db
                from app.models import Document, Folder
                from app.routers import (
                    all_folders,
                    build_folder_path_cache,
                    create_document_version,
                    document_path,
                    folder_relative_path,
                    get_or_create_blob_for_data,
                    now_utc,
                    subtree_folder_ids,
                )
                from app.storage import ensure_storage


                def timeout(_signum, _frame):
                    raise TimeoutError("folder traversal timed out")


                signal.signal(signal.SIGALRM, timeout)

                user = {
                    "id": "alice",
                    "name": "Alice",
                    "email": "alice@example.com",
                    "groups": [],
                    "is_admin": False,
                }

                init_db()
                ensure_storage()
                with SessionLocal() as db:
                    first = Folder(root_key="vault", parent_id=None, name="First", is_root=False)
                    second = Folder(root_key="vault", parent_id=None, name="Second", is_root=False)
                    db.add_all([first, second])
                    db.flush()
                    first.parent_id = second.id
                    second.parent_id = first.id
                    blob = get_or_create_blob_for_data(db, b"cycle", "text/plain")
                    doc = Document(
                        folder_id=first.id,
                        name="loop.txt",
                        created_by=user["id"],
                        created_by_name=user["name"],
                        latest_modified_by=user["id"],
                        latest_modified_at=now_utc(),
                    )
                    db.add(doc)
                    db.flush()
                    create_document_version(
                        db,
                        doc,
                        blob,
                        user,
                        {"ip": None, "user_agent": None},
                        "loop.txt",
                        "text/plain",
                        "Uploaded loop.txt",
                        "upload",
                    )
                    db.commit()
                    first_id = first.id
                    doc_id = doc.id

                with SessionLocal() as db:
                    folders = all_folders(db)
                    first = db.get(Folder, first_id)
                    doc = db.get(Document, doc_id)
                    assert first is not None
                    assert doc is not None

                    signal.alarm(2)
                    cache = build_folder_path_cache(folders)
                    relative = folder_relative_path(first)
                    subtree = subtree_folder_ids(first, folders)
                    path = document_path(doc, cache)
                    signal.alarm(0)

                    assert cache[first.id]
                    assert relative
                    assert subtree == {first.id, first.parent_id}
                    assert path.endswith("/loop.txt")
                """,
            )
            env = os.environ.copy()
            env["VAULT_DB_PATH"] = str(Path(temp_dir) / "vault.db")
            env["VAULT_OBJECTS_PATH"] = str(Path(temp_dir) / "objects")

            completed = subprocess.run(
                [sys.executable, "-c", script],
                check=False,
                cwd=Path(__file__).resolve().parents[1],
                env=env,
                stderr=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)


if __name__ == "__main__":
    unittest.main()
