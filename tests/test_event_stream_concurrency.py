import contextlib
import socket
import threading
import time
import unittest
from collections.abc import Iterator

import httpx
import uvicorn
from tests.support import auth_headers, vault_runtime

import app.routers as routers_module
from app.main import create_app


def free_tcp_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


@contextlib.contextmanager
def live_server() -> Iterator[str]:
    port = free_tcp_port()
    application = create_app(enable_ttl_sweeper=False)
    config = uvicorn.Config(
        application,
        host="127.0.0.1",
        port=port,
        log_level="error",
        lifespan="on",
    )
    server = uvicorn.Server(config)
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    deadline = time.monotonic() + 10
    while not server.started:
        if not thread.is_alive():
            raise RuntimeError("Uvicorn test server exited before startup")
        if time.monotonic() > deadline:
            raise RuntimeError("Uvicorn test server did not start")
        time.sleep(0.01)
    try:
        yield f"http://127.0.0.1:{port}"
    finally:
        server.should_exit = True
        thread.join(timeout=5)


def read_first_state_event(
    base_url: str,
    user_name: str,
    stream_errors: list[BaseException],
    received_lines: list[str] | None = None,
) -> None:
    try:
        with httpx.Client(timeout=5.0) as client:
            with client.stream(
                "GET",
                f"{base_url}/api/events/stream",
                headers=auth_headers(user_name, ["vault-admin"]),
            ) as response:
                if response.status_code != 200:
                    raise AssertionError(response.status_code)
                for line in response.iter_lines():
                    if line.startswith("id:"):
                        if received_lines is not None:
                            received_lines.append(line)
                        return
                raise AssertionError("event stream closed before a state event")
    except BaseException as exc:
        stream_errors.append(exc)


class EventStreamConcurrencyTests(unittest.TestCase):
    def test_blocked_event_poll_does_not_block_health_requests(self) -> None:
        original_state_events_after = routers_module.state_events_after
        entered_poll = threading.Event()
        release_poll = threading.Event()
        stream_error: list[BaseException] = []

        class FakeStateEvent:
            id = 1
            payload = {"resources": ["contents"]}

        def blocking_state_events_after(_last_id: int) -> list[object]:
            entered_poll.set()
            release_poll.wait(timeout=5)
            return [FakeStateEvent()]

        routers_module.state_events_after = blocking_state_events_after
        try:
            with vault_runtime(), live_server() as base_url:
                stream_thread = threading.Thread(
                    target=read_first_state_event,
                    args=(base_url, "stream-user", stream_error),
                    daemon=True,
                )
                stream_thread.start()
                self.assertTrue(entered_poll.wait(timeout=2), "event stream did not poll state")

                with httpx.Client(timeout=0.5) as client:
                    response = client.get(f"{base_url}/health")

                self.assertEqual(response.status_code, 200)
                self.assertEqual(response.text, "ok")
                release_poll.set()
                stream_thread.join(timeout=2)
                self.assertFalse(stream_thread.is_alive())
                self.assertEqual(stream_error, [])
        finally:
            release_poll.set()
            routers_module.state_events_after = original_state_events_after

    def test_ten_idle_event_streams_do_not_poll_sqlite_until_notified(self) -> None:
        original_state_events_after = routers_module.state_events_after
        stream_count = 10
        poll_count = 0
        poll_lock = threading.Lock()
        all_initial_polls = threading.Event()
        emit_event = threading.Event()
        stream_errors: list[BaseException] = []

        class FakeStateEvent:
            id = 1
            payload = {"resources": ["contents"]}

        def poll_total() -> int:
            with poll_lock:
                return poll_count

        def idle_state_events_after(_last_id: int) -> list[object]:
            nonlocal poll_count
            with poll_lock:
                poll_count += 1
                if poll_count == stream_count:
                    all_initial_polls.set()
            if emit_event.is_set():
                return [FakeStateEvent()]
            return []

        routers_module.state_events_after = idle_state_events_after
        try:
            with vault_runtime(), live_server() as base_url:
                threads = [
                    threading.Thread(
                        target=read_first_state_event,
                        args=(base_url, f"stream-user-{index}", stream_errors),
                        daemon=True,
                    )
                    for index in range(stream_count)
                ]
                for thread in threads:
                    thread.start()

                self.assertTrue(
                    all_initial_polls.wait(timeout=3),
                    "not all event streams reached their first poll",
                )
                time.sleep(0.75)
                self.assertEqual(poll_total(), stream_count)

                emit_event.set()
                routers_module.notify_state_event_committed()
                for thread in threads:
                    thread.join(timeout=2)
                    self.assertFalse(thread.is_alive())
                self.assertEqual(stream_errors, [])
        finally:
            emit_event.set()
            routers_module.notify_state_event_committed()
            routers_module.state_events_after = original_state_events_after

    def test_committed_state_event_reaches_stream_without_polling_delay(self) -> None:
        original_state_events_after = routers_module.state_events_after
        initial_poll = threading.Event()
        received_lines: list[str] = []
        stream_errors: list[BaseException] = []

        def tracking_state_events_after(last_id: int) -> list[object]:
            events = original_state_events_after(last_id)
            if not events:
                initial_poll.set()
            return events

        routers_module.state_events_after = tracking_state_events_after
        try:
            with vault_runtime() as ctx, live_server() as base_url:
                stream_thread = threading.Thread(
                    target=read_first_state_event,
                    args=(base_url, "stream-user", stream_errors, received_lines),
                    daemon=True,
                )
                stream_thread.start()
                self.assertTrue(initial_poll.wait(timeout=2), "event stream did not start polling")

                started_at = time.monotonic()
                with ctx.db() as db:
                    routers_module.record_state_change(db, "test.commit", ("contents",))
                    db.commit()

                stream_thread.join(timeout=0.45)
                elapsed = time.monotonic() - started_at
                self.assertFalse(stream_thread.is_alive())
                self.assertEqual(stream_errors, [])
                self.assertEqual(received_lines, ["id: 1"])
                self.assertLess(elapsed, 0.45)
        finally:
            routers_module.state_events_after = original_state_events_after


if __name__ == "__main__":
    unittest.main()
