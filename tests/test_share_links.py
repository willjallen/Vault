import datetime as dt
import unittest

from tests.support import add_permission, auth_headers, create_versioned_document, vault_test_client

from app.models import Document, Folder, ShareLink, VaultGroup
from app.routers import get_root_folder, now_utc


def create_child_folder(db, root: Folder, name: str) -> Folder:
    folder = Folder(root_key="vault", parent_id=root.id, parent=root, name=name, is_root=False)
    db.add(folder)
    db.flush()
    return folder


class ShareLinkTests(unittest.TestCase):
    def test_share_routes_resolve_current_targets_and_enforce_access(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
        artist_headers = auth_headers("artist", ["artists"])
        outsider_headers = auth_headers("outsider", ["outsiders"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                project = create_child_folder(db, root, "Art")

                artists = VaultGroup(name="artists")
                outsiders = VaultGroup(name="outsiders")
                db.add_all([artists, outsiders])
                db.flush()
                add_permission(db, root, artists)
                add_permission(db, root, outsiders)
                add_permission(db, project, artists)

                doc = Document(
                    folder_id=project.id,
                    name="mesh.blend",
                    created_by="admin",
                    created_by_name="Admin",
                    latest_modified_by="admin",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id
                folder_id = project.id

            doc_response = ctx.client.post(
                "/api/share-links",
                json={"target_type": "document", "document_id": doc_id},
                headers=admin_headers,
            )
            self.assertEqual(doc_response.status_code, 200, doc_response.text)
            doc_share = doc_response.json()
            self.assertTrue(doc_share["url"].startswith("http://testserver/s/"))
            self.assertEqual(doc_share["access_mode"], "internal")

            folder_response = ctx.client.post(
                "/api/share-links",
                json={"target_type": "folder", "path": "Art"},
                headers=admin_headers,
            )
            self.assertEqual(folder_response.status_code, 200, folder_response.text)
            folder_share = folder_response.json()

            with ctx.db() as db:
                project = db.get(Folder, folder_id)
                project.name = "Concepts"
                db.commit()

            resolved_doc = ctx.client.get(
                f"/api/share-links/{doc_share['code']}",
                headers=artist_headers,
            )
            self.assertEqual(resolved_doc.status_code, 200, resolved_doc.text)
            self.assertEqual(resolved_doc.json()["target_type"], "document")
            self.assertEqual(resolved_doc.json()["document_id"], doc_id)
            self.assertEqual(resolved_doc.json()["folder"], "Concepts")

            resolved_folder = ctx.client.get(
                f"/api/share-links/{folder_share['code']}",
                headers=artist_headers,
            )
            self.assertEqual(resolved_folder.status_code, 200, resolved_folder.text)
            self.assertEqual(resolved_folder.json()["target_type"], "folder")
            self.assertEqual(resolved_folder.json()["folder"], "Concepts")

            entry = ctx.client.get(
                f"/s/{doc_share['code']}",
                headers={**artist_headers, "X-Vault-Palette": "winui"},
            )
            self.assertEqual(entry.status_code, 200, entry.text)
            self.assertIn(f'"share_code": "{doc_share["code"]}"', entry.text)
            self.assertIn('"palette": "winui"', entry.text)
            self.assertNotIn("?folder=", entry.text)

            hidden = ctx.client.get(
                f"/api/share-links/{doc_share['code']}",
                headers=outsider_headers,
            )
            self.assertEqual(hidden.status_code, 404, hidden.text)

            bad_code = ctx.client.get("/api/share-links/not-a-valid-code!", headers=admin_headers)
            self.assertEqual(bad_code.status_code, 404)
            bad_entry = ctx.client.get("/s/not-a-valid-code!", headers=admin_headers)
            self.assertEqual(bad_entry.status_code, 404)

            with ctx.db() as db:
                link = db.query(ShareLink).filter_by(code=doc_share["code"]).one()
                link.disabled_at = now_utc()
                folder_link = db.query(ShareLink).filter_by(code=folder_share["code"]).one()
                folder_link.expires_at = now_utc() - dt.timedelta(seconds=1)
                db.commit()

            disabled = ctx.client.get(
                f"/api/share-links/{doc_share['code']}",
                headers=artist_headers,
            )
            self.assertEqual(disabled.status_code, 404)
            expired = ctx.client.get(
                f"/api/share-links/{folder_share['code']}",
                headers=artist_headers,
            )
            self.assertEqual(expired.status_code, 404)

    def test_share_creation_rejects_bad_and_inaccessible_targets(self) -> None:
        artist_headers = auth_headers("artist", ["artists"])
        outsider_headers = auth_headers("outsider", ["outsiders"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                project = create_child_folder(db, root, "Project")

                artists = VaultGroup(name="artists")
                outsiders = VaultGroup(name="outsiders")
                db.add_all([artists, outsiders])
                db.flush()
                add_permission(db, root, artists)
                add_permission(db, root, outsiders)
                add_permission(db, project, artists)

                doc = Document(
                    folder_id=project.id,
                    name="concept.png",
                    created_by="artist",
                    created_by_name="Artist",
                    latest_modified_by="artist",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id
                folder_id = project.id

            invalid_target = ctx.client.post(
                "/api/share-links",
                json={"target_type": "planet", "document_id": doc_id},
                headers=artist_headers,
            )
            self.assertEqual(invalid_target.status_code, 400)
            self.assertEqual(invalid_target.json()["detail"], "Invalid share target")

            missing_doc_id = ctx.client.post(
                "/api/share-links",
                json={"target_type": "document"},
                headers=artist_headers,
            )
            self.assertEqual(missing_doc_id.status_code, 400)
            self.assertEqual(missing_doc_id.json()["detail"], "Document id is required")

            missing_folder = ctx.client.post(
                "/api/share-links",
                json={"target_type": "folder", "path": "Missing"},
                headers=artist_headers,
            )
            self.assertEqual(missing_folder.status_code, 404)

            hidden_document = ctx.client.post(
                "/api/share-links",
                json={"target_type": "document", "document_id": doc_id},
                headers=outsider_headers,
            )
            self.assertEqual(hidden_document.status_code, 404)

            hidden_folder = ctx.client.post(
                "/api/share-links",
                json={"target_type": "folder", "path": "Project"},
                headers=outsider_headers,
            )
            self.assertEqual(hidden_folder.status_code, 404)

            visible_document = ctx.client.post(
                "/api/share-links",
                json={"target_type": "document", "document_id": doc_id},
                headers=artist_headers,
            )
            self.assertEqual(visible_document.status_code, 200, visible_document.text)

            visible_folder = ctx.client.post(
                "/api/share-links",
                json={"target_type": "folder", "folder_id": folder_id},
                headers=artist_headers,
            )
            self.assertEqual(visible_folder.status_code, 200, visible_folder.text)

    def test_folder_share_stats_exclude_inaccessible_descendants(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])
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

                create_versioned_document(db, project, name="visible.txt", data=b"ok")
                create_versioned_document(db, private, name="secret.txt", data=b"topsecret")
                db.commit()
                project_id = project.id

            share = ctx.client.post(
                "/api/share-links",
                json={"target_type": "folder", "folder_id": project_id},
                headers=admin_headers,
            )
            self.assertEqual(share.status_code, 200, share.text)

            resolved = ctx.client.get(
                f"/api/share-links/{share.json()['code']}",
                headers=artist_headers,
            )
            self.assertEqual(resolved.status_code, 200, resolved.text)
            self.assertEqual(resolved.json()["folder_item"]["size_bytes"], len(b"ok"))

    def test_deleted_targets_resolve_as_not_found(self) -> None:
        admin_headers = auth_headers("admin", ["vault-admin"])

        with vault_test_client() as ctx:
            with ctx.db() as db:
                root = get_root_folder(db, "vault")
                folder = create_child_folder(db, root, "Temp")
                doc = Document(
                    folder_id=folder.id,
                    name="delete-me.txt",
                    created_by="admin",
                    created_by_name="Admin",
                    latest_modified_by="admin",
                    latest_modified_at=now_utc(),
                )
                db.add(doc)
                db.commit()
                doc_id = doc.id
                folder_id = folder.id

            doc_share = ctx.client.post(
                "/api/share-links",
                json={"target_type": "document", "document_id": doc_id},
                headers=admin_headers,
            ).json()
            folder_share = ctx.client.post(
                "/api/share-links",
                json={"target_type": "folder", "path": "Temp"},
                headers=admin_headers,
            ).json()

            with ctx.db() as db:
                db.delete(db.get(Document, doc_id))
                db.delete(db.get(Folder, folder_id))
                db.commit()

            doc_resolve = ctx.client.get(
                f"/api/share-links/{doc_share['code']}",
                headers=admin_headers,
            )
            self.assertEqual(doc_resolve.status_code, 404)
            folder_resolve = ctx.client.get(
                f"/api/share-links/{folder_share['code']}",
                headers=admin_headers,
            )
            self.assertEqual(folder_resolve.status_code, 404)

            with ctx.db() as db:
                self.assertEqual(db.query(ShareLink).count(), 0)


if __name__ == "__main__":
    unittest.main()
