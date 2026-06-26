import unittest

from tests.support import (
    add_permission,
    auth_headers,
    create_versioned_document,
    upload_file_via_session,
    vault_test_client,
)

import app.config as config_module
from app.models import Document, Folder, VaultGroup
from app.routers import (
    create_document_version,
    get_or_create_blob_for_data,
    get_root_folder,
    now_utc,
)


def create_child_folder(db, root: Folder, name: str) -> Folder:
    folder = Folder(root_key="vault", parent_id=root.id, parent=root, name=name, is_root=False)
    db.add(folder)
    db.flush()
    return folder


class HttpApiContractTests(unittest.TestCase):
    def test_security_headers_are_applied_to_http_responses(self) -> None:
        original_public_url = config_module.PUBLIC_URL
        try:
            with vault_test_client() as ctx:
                response = ctx.client.get("/health")
                self.assertEqual(response.status_code, 200)
                self.assertEqual(response.headers["x-content-type-options"], "nosniff")
                self.assertEqual(response.headers["x-frame-options"], "DENY")
                self.assertEqual(response.headers["referrer-policy"], "no-referrer")
                self.assertIn("frame-ancestors 'none'", response.headers["content-security-policy"])
                self.assertNotIn("strict-transport-security", response.headers)

                config_module.PUBLIC_URL = "https://vault.example.com"
                secure_response = ctx.client.get("/health")
                self.assertIn("max-age=", secure_response.headers["strict-transport-security"])
        finally:
            config_module.PUBLIC_URL = original_public_url

    def test_file_api_permissions_over_real_http(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        viewer_headers = auth_headers("viewer", ["viewers"])
        reader_headers = auth_headers("reader", ["readers"])
        writer_headers = auth_headers("writer", ["writers"])
        outsider_headers = auth_headers("outsider", ["outsiders"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                project = create_child_folder(db, root, "Project")

                viewers = VaultGroup(name="viewers")
                readers = VaultGroup(name="readers")
                writers = VaultGroup(name="writers")
                outsiders = VaultGroup(name="outsiders")
                db.add_all([viewers, readers, writers, outsiders])
                db.flush()
                for group in (viewers, readers, writers, outsiders):
                    add_permission(db, root, group)

                add_permission(db, project, viewers, read=False)
                add_permission(db, project, readers)
                add_permission(db, project, writers, write=True)

                blob = get_or_create_blob_for_data(db, b"secret", "text/plain")
                doc = Document(
                    folder_id=project.id,
                    name="plan.txt",
                    created_by="admin",
                    created_by_name="Admin",
                    latest_modified_by="admin",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.flush()
                create_document_version(
                    db,
                    doc,
                    blob,
                    {
                        "id": "admin",
                        "vault_user_id": 0,
                        "issuer": "test",
                        "subject": "admin",
                        "name": "Admin",
                        "email": "admin@example.com",
                        "groups": ["vault-admin"],
                        "is_admin": True,
                    },
                    {"ip": None, "user_agent": None},
                    "plan.txt",
                    "text/plain",
                    "Uploaded plan.txt",
                    "upload",
                )
                db.commit()
                doc_id = doc.id

            viewer_contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Project"},
                headers=viewer_headers,
            )
            self.assertEqual(viewer_contents.status_code, 200, viewer_contents.text)
            [doc_row] = viewer_contents.json()["documents"]
            self.assertEqual(doc_row["name"], "plan.txt")
            self.assertEqual(
                doc_row["access"],
                {
                    "visible": True,
                    "read": False,
                    "write": False,
                },
            )

            outsider_contents = ctx.client.get(
                "/api/folders/contents",
                params={"folder": "Project"},
                headers=outsider_headers,
            )
            self.assertEqual(outsider_contents.status_code, 404)

            viewer_download = ctx.client.post(
                "/api/download",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=viewer_headers,
            )
            self.assertEqual(viewer_download.status_code, 403)

            reader_download = ctx.client.post(
                "/api/download",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=reader_headers,
            )
            self.assertEqual(reader_download.status_code, 200, reader_download.text)
            self.assertEqual(reader_download.content, b"secret")
            self.assertIn("plan.txt", reader_download.headers["content-disposition"])

            reader_upload = ctx.client.post(
                "/documents",
                data={"folder": "Project"},
                files={"file": ("reader.txt", b"reader", "text/plain")},
                headers=reader_headers,
            )
            self.assertEqual(reader_upload.status_code, 410)

            reader_session_upload = upload_file_via_session(
                ctx.client,
                headers=reader_headers,
                filename="reader.txt",
                data=b"reader",
                folder="Project",
            )
            self.assertEqual(reader_session_upload.status_code, 403)

            writer_upload = upload_file_via_session(
                ctx.client,
                headers=writer_headers,
                filename="writer.txt",
                data=b"writer",
                folder="Project",
            )
            self.assertEqual(writer_upload.status_code, 200, writer_upload.text)
            self.assertEqual(writer_upload.json()["path"], "Project/writer.txt")

            reader_lock = ctx.client.post(
                "/api/lock",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=reader_headers,
            )
            self.assertEqual(reader_lock.status_code, 200, reader_lock.text)
            self.assertEqual(
                reader_lock.json()["failed"][0]["detail"],
                "Insufficient document access",
            )

            writer_lock = ctx.client.post(
                "/api/lock",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=writer_headers,
            )
            self.assertEqual(writer_lock.status_code, 200, writer_lock.text)
            self.assertEqual(
                writer_lock.json()["ok"][0]["item"],
                {"type": "document", "id": doc_id},
            )

            writer_unlock = ctx.client.post(
                "/api/unlock",
                json={"items": [{"type": "document", "id": doc_id}]},
                headers=writer_headers,
            )
            self.assertEqual(writer_unlock.status_code, 200, writer_unlock.text)
            self.assertEqual(writer_unlock.json()["ok"][0]["detail"], "Unlocked")

            root_with_query = ctx.client.get("/?folder=Project", headers=admin_headers)
            self.assertEqual(root_with_query.status_code, 200, root_with_query.text)
            self.assertIn('"current_folder": ""', root_with_query.text)
            self.assertNotIn('"current_folder": "Project"', root_with_query.text)
            self.assertNotIn("?folder=", root_with_query.text)

    def test_folder_properties_hide_inaccessible_descendant_stats(self) -> None:
        artist_headers = auth_headers("artist", ["artists"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                project = create_child_folder(db, root, "Project")
                private = create_child_folder(db, project, "Private")

                artists = VaultGroup(name="artists")
                confidential = VaultGroup(name="confidential")
                db.add_all([artists, confidential])
                db.flush()
                add_permission(db, root, artists)
                add_permission(db, project, artists)
                add_permission(db, private, confidential)

                create_versioned_document(db, private, name="secret.txt", data=b"secret")
                db.commit()

            properties = ctx.client.get(
                "/api/folders/properties",
                params={"path": "Project"},
                headers=artist_headers,
            )
            self.assertEqual(properties.status_code, 200, properties.text)
            payload = properties.json()
            self.assertEqual(payload["counts"], {"folders": 0, "documents": 0})
            self.assertEqual(payload["size_bytes"], 0)
            self.assertIsNone(payload["latest_by"])
            self.assertIsNone(payload["modified_at"])
            self.assertEqual(payload["permissions"], [])
            self.assertEqual(payload["available_groups"], [])


if __name__ == "__main__":
    unittest.main()
