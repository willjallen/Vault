import signal
import unittest

from tests.support import create_versioned_document, user_context, vault_runtime

from app.db import SessionLocal
from app.models import Document, Folder
from app.routers import (
    all_folders,
    build_folder_path_cache,
    document_path,
    folder_relative_path,
    subtree_folder_ids,
)


class FolderCycleStateTests(unittest.TestCase):
    def test_folder_path_helpers_tolerate_corrupt_parent_cycle(self) -> None:
        def timeout(_signum, _frame):
            raise TimeoutError("folder traversal timed out")

        user = user_context("alice", groups=[], is_admin=False)

        with vault_runtime():
            with SessionLocal() as db:
                first = Folder(root_key="vault", parent_id=None, name="First", is_root=False)
                second = Folder(root_key="vault", parent_id=None, name="Second", is_root=False)
                db.add_all([first, second])
                db.flush()
                first.parent_id = second.id
                second.parent_id = first.id
                doc = create_versioned_document(
                    db,
                    first,
                    name="loop.txt",
                    data=b"cycle",
                    actor=user,
                )
                db.commit()
                first_id = first.id
                doc_id = doc.id

            with SessionLocal() as db:
                folders = all_folders(db)
                first = db.get(Folder, first_id)
                doc = db.get(Document, doc_id)
                self.assertIsNotNone(first)
                self.assertIsNotNone(doc)

                signal.signal(signal.SIGALRM, timeout)
                signal.alarm(2)
                try:
                    cache = build_folder_path_cache(folders)
                    relative = folder_relative_path(first)
                    subtree = subtree_folder_ids(first, folders)
                    path = document_path(doc, cache)
                finally:
                    signal.alarm(0)

                self.assertTrue(cache[first.id])
                self.assertTrue(relative)
                self.assertEqual(subtree, {first.id, first.parent_id})
                self.assertTrue(path.endswith("/loop.txt"))


if __name__ == "__main__":
    unittest.main()
