#!/usr/bin/env python3
"""Benchmark Vault upload/download transfer paths.

This is an optional one-off utility. It intentionally uses only the Python
standard library so the repository does not require a virtualenv, requirements
file, or Python lint/test tooling for normal development.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import closing
from dataclasses import asdict, dataclass, replace
from pathlib import Path
from typing import Any, Iterable

MIB = 1024 * 1024
DEFAULT_BODY_BLOCK_BYTES = 256 * 1024
LOCAL_DIRECT_PROFILE = "local-direct"
RUST_SINK_MODE = "rust-sink"
BENCHMARK_SERVER_ENV_KEYS = (
    "BASE_DOMAIN",
    "VAULT_BENCH_ROUTES",
    "VAULT_BENCH_SINK_DIR",
    "VAULT_BENCH_SINK_HASH",
    "VAULT_BENCH_SINK_WRITE",
    "VAULT_AUTH_MODE",
    "VAULT_DATA_DIR",
    "VAULT_DB_PATH",
    "VAULT_DEV_AUTH",
    "VAULT_DEV_MODE",
    "VAULT_DOCKER_RUNTIME",
    "VAULT_GZIP_MINIMUM_SIZE",
    "VAULT_HOST",
    "VAULT_MAX_UPLOAD_BYTES",
    "VAULT_OBJECTS_PATH",
    "VAULT_PORT",
    "VAULT_SECURITY_HEADERS_ENABLED",
    "VAULT_SESSION_COOKIE_SECURE",
    "VAULT_SESSION_SECRET",
    "VAULT_STATIC_DIR",
    "VAULT_STORAGE_BACKEND",
    "VAULT_TRANSFER_CHUNK_BYTES",
    "VAULT_TRANSFER_SESSION_TTL_SECONDS",
    "VAULT_TRANSFERS_PATH",
)


@dataclass(frozen=True)
class BenchCase:
    name: str
    users: int
    file_mib: int
    workers: int
    chunk_mib: int | None = None


@dataclass
class CaseResult:
    name: str
    mode: str
    server: str
    users: int
    file_mib: int
    workers: int
    chunk_mib: int | None
    upload_wall_seconds: float
    upload_mib_per_second: float
    part_wall_seconds: float | None
    part_mib_per_second: float | None
    complete_wall_seconds: float | None
    download_wall_seconds: float | None
    download_mib_per_second: float | None
    part_count: int
    part_min_seconds: float
    part_p50_seconds: float
    part_p95_seconds: float
    part_max_seconds: float
    server_cpu_seconds: float | None
    server_rss_mib: float | None
    server_peak_rss_mib: float | None


@dataclass(frozen=True)
class ThroughputThreshold:
    case_name: str | None
    mode: str | None
    server: str | None
    min_upload_mib_per_second: float | None = None
    min_download_mib_per_second: float | None = None


@dataclass(frozen=True)
class ProcessUsage:
    cpu_seconds: float
    rss_mib: float | None
    peak_rss_mib: float | None


@dataclass(frozen=True)
class HttpResponse:
    status: int
    headers: dict[str, str]
    body: bytes

    def json(self) -> Any:
        return json.loads(self.body.decode("utf-8"))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode",
        choices=("app", RUST_SINK_MODE, "both"),
        default="app",
        help="Benchmark the real Rust app, the Rust sink route, or both.",
    )
    parser.add_argument(
        "--case",
        action="append",
        choices=tuple(default_cases()),
        help="Case name to run. May be passed more than once. Defaults to all cases.",
    )
    parser.add_argument("--json", type=Path, help="Write machine-readable results to this path.")
    parser.add_argument(
        "--body-block-kib",
        type=int,
        default=DEFAULT_BODY_BLOCK_BYTES // 1024,
        help="Client request-body generator chunk size.",
    )
    parser.add_argument("--quiet", action="store_true", help="Only print the final JSON payload.")
    parser.add_argument(
        "--part-checksum",
        action="store_true",
        help="Send X-Upload-Sha256 for each app upload part.",
    )
    parser.add_argument(
        "--client-rate-mib",
        type=float,
        default=None,
        help="Optional per-request upload body rate cap in MiB/s.",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=None,
        help="Override upload workers/client parallelism for selected cases.",
    )
    parser.add_argument(
        "--sink-no-checksum",
        action="store_true",
        help="In Rust sink mode, read request bodies without hashing them.",
    )
    parser.add_argument(
        "--sink-write",
        action="store_true",
        help="In Rust sink mode, write request bodies to temporary files.",
    )
    parser.add_argument(
        "--rust-bin",
        type=Path,
        help=(
            "Path to a prebuilt vault-server binary. Defaults to target/release/vault-server "
            "when present, otherwise cargo run --release -p vault-server."
        ),
    )
    parser.add_argument(
        "--startup-timeout",
        type=float,
        default=120.0,
        help="Seconds to wait for the benchmark server to become healthy.",
    )
    parser.add_argument(
        "--runner",
        choices=("direct", "docker"),
        default="direct",
        help="Run the benchmark server directly on the host or inside a Docker container.",
    )
    parser.add_argument(
        "--docker-image",
        default=os.getenv("VAULT_BENCH_DOCKER_IMAGE", "vault-bench:local"),
        help="Docker image to run when --runner=docker.",
    )
    parser.add_argument(
        "--docker-build",
        action="store_true",
        help="Build --docker-image from the repository Dockerfile before running benchmarks.",
    )
    parser.add_argument(
        "--target-profile",
        action="append",
        choices=(LOCAL_DIRECT_PROFILE,),
        default=[],
        help="Enforce a named throughput target profile against matching benchmark results.",
    )
    parser.add_argument(
        "--min-upload-mibps",
        type=float,
        default=None,
        help="Fail if any matching result uploads below this aggregate MiB/s floor.",
    )
    parser.add_argument(
        "--min-download-mibps",
        type=float,
        default=None,
        help="Fail if any matching app result downloads below this aggregate MiB/s floor.",
    )
    return parser.parse_args()


def default_cases() -> dict[str, BenchCase]:
    return {
        "kevin-38m": BenchCase("kevin-38m", users=1, file_mib=38, workers=16),
        "kevin-114m": BenchCase("kevin-114m", users=1, file_mib=109, workers=16),
        "single-128m": BenchCase("single-128m", users=1, file_mib=128, workers=16),
        "ten-64m": BenchCase("ten-64m", users=10, file_mib=64, workers=16),
        "ten-64m-4m-parts": BenchCase(
            "ten-64m-4m-parts",
            users=10,
            file_mib=64,
            workers=16,
            chunk_mib=4,
        ),
    }


def local_direct_thresholds() -> list[ThroughputThreshold]:
    return [
        ThroughputThreshold("single-128m", "app", "rust", 400.0, 900.0),
        ThroughputThreshold("ten-64m", "app", "rust", 500.0, 900.0),
        ThroughputThreshold("ten-64m-4m-parts", "app", "rust", 450.0, 900.0),
    ]


def configured_thresholds(args: argparse.Namespace) -> list[ThroughputThreshold]:
    thresholds: list[ThroughputThreshold] = []
    for profile in args.target_profile:
        if profile == LOCAL_DIRECT_PROFILE:
            thresholds.extend(local_direct_thresholds())
    if args.min_upload_mibps is not None or args.min_download_mibps is not None:
        thresholds.append(
            ThroughputThreshold(
                case_name=None,
                mode=None,
                server=None,
                min_upload_mib_per_second=args.min_upload_mibps,
                min_download_mib_per_second=args.min_download_mibps,
            ),
        )
    return thresholds


def threshold_matches(result: CaseResult, threshold: ThroughputThreshold) -> bool:
    return (
        (threshold.case_name is None or threshold.case_name == result.name)
        and (threshold.mode is None or threshold.mode == result.mode)
        and (threshold.server is None or threshold.server == result.server)
    )


def threshold_failures(
    results: list[CaseResult],
    thresholds: list[ThroughputThreshold],
) -> list[str]:
    failures: list[str] = []
    for threshold in thresholds:
        for result in results:
            if not threshold_matches(result, threshold):
                continue
            if (
                threshold.min_upload_mib_per_second is not None
                and result.upload_mib_per_second < threshold.min_upload_mib_per_second
            ):
                failures.append(
                    f"{result.mode}:{result.name} upload "
                    f"{result.upload_mib_per_second:.1f}MiB/s below "
                    f"{threshold.min_upload_mib_per_second:.1f}MiB/s"
                )
            if threshold.min_download_mib_per_second is None:
                continue
            if result.download_mib_per_second is None:
                failures.append(
                    f"{result.mode}:{result.name} download missing below "
                    f"{threshold.min_download_mib_per_second:.1f}MiB/s"
                )
                continue
            if result.download_mib_per_second < threshold.min_download_mib_per_second:
                failures.append(
                    f"{result.mode}:{result.name} download "
                    f"{result.download_mib_per_second:.1f}MiB/s below "
                    f"{threshold.min_download_mib_per_second:.1f}MiB/s"
                )
    return failures


def percentile(values: list[float], percentile_value: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round((len(ordered) - 1) * percentile_value)))
    return ordered[index]


def free_port() -> int:
    with closing(socket.socket(socket.AF_INET, socket.SOCK_STREAM)) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def process_usage(pid: int) -> ProcessUsage | None:
    proc_dir = Path("/proc") / str(pid)
    if not proc_dir.exists():
        return None
    try:
        stat = (proc_dir / "stat").read_text(encoding="utf-8")
        status = (proc_dir / "status").read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        stat_fields = stat.rsplit(") ", 1)[1].split()
        utime_ticks = int(stat_fields[11])
        stime_ticks = int(stat_fields[12])
        ticks_per_second = os.sysconf("SC_CLK_TCK")
    except (IndexError, OSError, ValueError):
        return None
    return ProcessUsage(
        cpu_seconds=(utime_ticks + stime_ticks) / ticks_per_second,
        rss_mib=status_value_mib(status, "VmRSS"),
        peak_rss_mib=status_value_mib(status, "VmHWM"),
    )


def status_value_mib(status: str, key: str) -> float | None:
    prefix = f"{key}:"
    for line in status.splitlines():
        if not line.startswith(prefix):
            continue
        parts = line.removeprefix(prefix).strip().split()
        if not parts:
            return None
        try:
            value = float(parts[0])
        except ValueError:
            return None
        unit = parts[1].lower() if len(parts) > 1 else "kb"
        if unit == "kb":
            return value / 1024.0
        if unit == "mb":
            return value
        if unit == "gb":
            return value * 1024.0
        return None
    return None


def with_process_usage(
    result: CaseResult,
    before: ProcessUsage | None,
    after: ProcessUsage | None,
) -> CaseResult:
    server_cpu_seconds = None
    if before is not None and after is not None:
        server_cpu_seconds = max(0.0, after.cpu_seconds - before.cpu_seconds)
    return replace(
        result,
        server_cpu_seconds=server_cpu_seconds,
        server_rss_mib=after.rss_mib if after is not None else None,
        server_peak_rss_mib=after.peak_rss_mib if after is not None else None,
    )


def auth_headers(user: str) -> dict[str, str]:
    return {
        "Remote-User": user,
        "Remote-Name": user.title(),
        "Remote-Email": f"{user}@example.com",
        "Remote-Groups": "vault-admin",
    }


def repeated_sha256(size: int, body_block: bytes) -> str:
    digest = hashlib.sha256()
    remaining = size
    while remaining:
        chunk = body_block[: min(remaining, len(body_block))]
        digest.update(chunk)
        remaining -= len(chunk)
    return digest.hexdigest()


def repeated_body(
    size: int,
    body_block: bytes,
    rate_mib_per_second: float | None,
) -> Iterable[bytes]:
    remaining = size
    sent = 0
    started = time.perf_counter()
    while remaining:
        chunk = body_block[: min(remaining, len(body_block))]
        yield chunk
        remaining -= len(chunk)
        sent += len(chunk)
        if rate_mib_per_second and rate_mib_per_second > 0:
            expected_elapsed = sent / (rate_mib_per_second * MIB)
            actual_elapsed = time.perf_counter() - started
            if expected_elapsed > actual_elapsed:
                time.sleep(expected_elapsed - actual_elapsed)


def request_bytes(
    host: str,
    port: int,
    method: str,
    path: str,
    *,
    headers: dict[str, str] | None = None,
    body: bytes | Iterable[bytes] | None = None,
    timeout: float = 300.0,
) -> HttpResponse:
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        conn.putrequest(method, path, skip_host=False, skip_accept_encoding=True)
        for key, value in (headers or {}).items():
            conn.putheader(key, value)
        conn.putheader("Connection", "close")
        conn.endheaders()
        if body is not None:
            if isinstance(body, bytes):
                conn.send(body)
            else:
                for chunk in body:
                    if chunk:
                        conn.send(chunk)
        response = conn.getresponse()
        response_headers = {key.lower(): value for key, value in response.getheaders()}
        return HttpResponse(response.status, response_headers, response.read())
    finally:
        conn.close()


def json_request(
    host: str,
    port: int,
    method: str,
    path: str,
    *,
    headers: dict[str, str] | None = None,
    payload: Any | None = None,
    timeout: float = 300.0,
) -> HttpResponse:
    body = b"" if payload is None else json.dumps(payload).encode("utf-8")
    request_headers = {
        "Content-Type": "application/json",
        "Content-Length": str(len(body)),
        **(headers or {}),
    }
    return request_bytes(
        host,
        port,
        method,
        path,
        headers=request_headers,
        body=body,
        timeout=timeout,
    )


def raise_for_status(response: HttpResponse, context: str) -> None:
    if 200 <= response.status < 300:
        return
    body = response.body.decode("utf-8", "replace")
    raise RuntimeError(f"{context} failed with HTTP {response.status}: {body}")


def wait_health(host: str, port: int, proc: subprocess.Popen[bytes], timeout_seconds: float) -> None:
    deadline = time.perf_counter() + timeout_seconds
    while time.perf_counter() < deadline:
        if proc.poll() is not None:
            stderr = proc.stderr.read().decode("utf-8", "replace") if proc.stderr else ""
            raise RuntimeError(f"server exited early: {stderr}")
        try:
            response = request_bytes(host, port, "GET", "/health", timeout=1.0)
            if response.status == 200 and response.body == b"ok":
                return
        except OSError:
            pass
        time.sleep(0.1)
    raise TimeoutError("server did not become healthy")


def server_env(temp_dir: Path, chunk_mib: int | None, port: int) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "BASE_DOMAIN": "localhost",
            "VAULT_HOST": "127.0.0.1",
            "VAULT_PORT": str(port),
            "VAULT_AUTH_MODE": "headers",
            "VAULT_DEV_AUTH": "0",
            "VAULT_DEV_MODE": "0",
            "VAULT_DATA_DIR": str(temp_dir),
            "VAULT_DB_PATH": str(temp_dir / "vault.db"),
            "VAULT_OBJECTS_PATH": str(temp_dir / "objects"),
            "VAULT_TRANSFERS_PATH": str(temp_dir / "transfers"),
            "VAULT_STATIC_DIR": str(Path.cwd() / "app" / "static"),
            "VAULT_STORAGE_BACKEND": "local",
            "VAULT_SESSION_SECRET": "benchmark-session-secret",
            "VAULT_SESSION_COOKIE_SECURE": "auto",
            "VAULT_MAX_UPLOAD_BYTES": str(5 * 1024 * MIB),
            "VAULT_TRANSFER_SESSION_TTL_SECONDS": "86400",
            "VAULT_SECURITY_HEADERS_ENABLED": "0",
            "VAULT_GZIP_MINIMUM_SIZE": "0",
            "VAULT_BENCH_SINK_DIR": str(temp_dir / "sink"),
        },
    )
    if chunk_mib is not None:
        env["VAULT_TRANSFER_CHUNK_BYTES"] = str(chunk_mib * MIB)
    return env


def container_server_env(chunk_mib: int | None) -> dict[str, str]:
    env = server_env(Path("/data"), chunk_mib, 8000)
    env.update(
        {
            "VAULT_HOST": "0.0.0.0",
            "VAULT_STATIC_DIR": "/app/app/static",
            "VAULT_DOCKER_RUNTIME": "1",
        },
    )
    return env


def docker_env_args(env: dict[str, str]) -> list[str]:
    args: list[str] = []
    for key in BENCHMARK_SERVER_ENV_KEYS:
        if key in env:
            args.extend(["--env", f"{key}={env[key]}"])
    return args


def rust_server_command(rust_bin: Path | None) -> list[str]:
    if rust_bin is not None:
        return [str(rust_bin)]
    candidate = Path("target") / "release" / "vault-server"
    if os.name == "nt":
        candidate = candidate.with_suffix(".exe")
    if candidate.exists():
        return [str(candidate)]
    return ["cargo", "run", "--release", "-p", "vault-server", "--"]


def docker_server_command(
    *,
    image: str,
    host_port: int,
    data_dir: Path,
    chunk_mib: int | None,
) -> list[str]:
    return [
        "docker",
        "run",
        "--rm",
        "--publish",
        f"127.0.0.1:{host_port}:8000",
        "--volume",
        f"{data_dir}:/data",
        *docker_env_args(container_server_env(chunk_mib)),
        image,
    ]


def docker_build_command(image: str) -> list[str]:
    return ["docker", "build", "--tag", image, "."]


def build_docker_image(image: str) -> None:
    subprocess.run(docker_build_command(image), cwd=Path.cwd(), check=True)


def start_server(
    *,
    mode: str,
    runner: str,
    port: int,
    temp_dir: Path,
    chunk_mib: int | None,
    sink_checksum: bool,
    sink_write: bool,
    rust_bin: Path | None,
    docker_image: str,
) -> subprocess.Popen[bytes]:
    if runner == "docker":
        command = docker_server_command(
            image=docker_image,
            host_port=port,
            data_dir=temp_dir,
            chunk_mib=chunk_mib,
        )
        env = os.environ.copy()
    else:
        env = server_env(temp_dir, chunk_mib, port)
        env["VAULT_BENCH_SINK_HASH"] = "1" if sink_checksum else "0"
        env["VAULT_BENCH_SINK_WRITE"] = "1" if sink_write else "0"
        env["VAULT_BENCH_ROUTES"] = "1" if mode == RUST_SINK_MODE else "0"
        command = rust_server_command(rust_bin)
    return subprocess.Popen(
        command,
        cwd=Path.cwd(),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def benchmark_sink_case(
    *,
    host: str,
    port: int,
    case: BenchCase,
    body_block: bytes,
    client_rate_mib: float | None,
    sink_checksum: bool,
) -> CaseResult:
    file_size = case.file_mib * MIB
    total_mib = case.users * case.file_mib
    part_size = case.chunk_mib * MIB if case.chunk_mib else min(32 * MIB, file_size)
    part_durations: list[float] = []
    lock = threading.Lock()
    started = time.perf_counter()

    def put_part(size: int) -> None:
        expected = repeated_sha256(size, body_block) if sink_checksum else None
        t0 = time.perf_counter()
        response = request_bytes(
            host,
            port,
            "PUT",
            "/api/bench/sink",
            headers={"Content-Length": str(size)},
            body=repeated_body(size, body_block, client_rate_mib),
        )
        elapsed = time.perf_counter() - t0
        raise_for_status(response, "sink upload")
        if sink_checksum and response.json()["sha256"] != expected:
            raise AssertionError("sink checksum mismatch")
        with lock:
            part_durations.append(elapsed)

    with concurrent.futures.ThreadPoolExecutor(max_workers=case.users * case.workers) as executor:
        futures = []
        for _ in range(case.users):
            for offset in range(0, file_size, part_size):
                futures.append(executor.submit(put_part, min(part_size, file_size - offset)))
        for future in concurrent.futures.as_completed(futures):
            future.result()

    upload_wall = time.perf_counter() - started
    return CaseResult(
        name=case.name,
        mode=RUST_SINK_MODE,
        server="rust",
        users=case.users,
        file_mib=case.file_mib,
        workers=case.workers,
        chunk_mib=case.chunk_mib,
        upload_wall_seconds=upload_wall,
        upload_mib_per_second=total_mib / upload_wall,
        part_wall_seconds=upload_wall,
        part_mib_per_second=total_mib / upload_wall,
        complete_wall_seconds=None,
        download_wall_seconds=None,
        download_mib_per_second=None,
        part_count=len(part_durations),
        part_min_seconds=min(part_durations),
        part_p50_seconds=percentile(part_durations, 0.50),
        part_p95_seconds=percentile(part_durations, 0.95),
        part_max_seconds=max(part_durations),
        server_cpu_seconds=None,
        server_rss_mib=None,
        server_peak_rss_mib=None,
    )


def benchmark_app_case(
    *,
    host: str,
    port: int,
    case: BenchCase,
    body_block: bytes,
    part_checksum: bool,
    client_rate_mib: float | None,
) -> CaseResult:
    file_size = case.file_mib * MIB
    total_mib = case.users * case.file_mib
    final_sha = repeated_sha256(file_size, body_block)
    part_sha_cache: dict[int, str] = {}
    part_durations: list[float] = []
    part_started_at: list[float] = []
    part_finished_at: list[float] = []
    complete_started_at: list[float] = []
    complete_finished_at: list[float] = []
    doc_ids: list[tuple[int, dict[str, str]]] = []
    lock = threading.Lock()
    started = time.perf_counter()

    def upload_user(index: int) -> None:
        headers = auth_headers(f"bench{index}")
        session_response = json_request(
            host,
            port,
            "POST",
            "/api/uploads",
            headers=headers,
            payload={
                "filename": f"bench-{index}-{case.file_mib}m.bin",
                "folder": "",
                "mime_type": "application/octet-stream",
                "mode": "create",
                "size_bytes": file_size,
                "client_upload_parallelism": case.workers,
            },
        )
        raise_for_status(session_response, "create upload session")
        session = session_response.json()
        session_id = session["id"]
        token = session["upload_token"]
        chunk_size = int(session["chunk_size"])

        def put_part(part_number: int, offset: int, size: int) -> None:
            part_headers = {
                "Content-Type": "application/octet-stream",
                "Content-Length": str(size),
                "X-Upload-Offset": str(offset),
                "X-Upload-Size": str(size),
                "X-Upload-Token": token,
            }
            if part_checksum:
                part_headers["X-Upload-Sha256"] = part_sha_cache.setdefault(
                    size,
                    repeated_sha256(size, body_block),
                )
            t0 = time.perf_counter()
            response = request_bytes(
                host,
                port,
                "PUT",
                f"/api/uploads/{session_id}/parts/{part_number}",
                headers=part_headers,
                body=repeated_body(size, body_block, client_rate_mib),
            )
            elapsed = time.perf_counter() - t0
            raise_for_status(response, f"upload part {part_number}")
            with lock:
                part_durations.append(elapsed)

        part_started = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(max_workers=case.workers) as executor:
            futures = []
            for part_number, offset in enumerate(range(0, file_size, chunk_size), start=1):
                size = min(chunk_size, file_size - offset)
                futures.append(executor.submit(put_part, part_number, offset, size))
            for future in concurrent.futures.as_completed(futures):
                future.result()
        part_finished = time.perf_counter()

        complete_started = time.perf_counter()
        complete_response = json_request(
            host,
            port,
            "POST",
            f"/api/uploads/{session_id}/complete",
            headers=headers,
            payload={"sha256": final_sha},
        )
        complete_finished = time.perf_counter()
        raise_for_status(complete_response, "complete upload session")
        with lock:
            part_started_at.append(part_started)
            part_finished_at.append(part_finished)
            complete_started_at.append(complete_started)
            complete_finished_at.append(complete_finished)
            doc_ids.append((int(complete_response.json()["id"]), headers))

    with concurrent.futures.ThreadPoolExecutor(max_workers=case.users) as executor:
        futures = [executor.submit(upload_user, index) for index in range(case.users)]
        for future in concurrent.futures.as_completed(futures):
            future.result()

    upload_wall = time.perf_counter() - started
    download_started = time.perf_counter()

    def download_doc(doc_id: int, headers: dict[str, str]) -> int:
        conn = http.client.HTTPConnection(host, port, timeout=300.0)
        try:
            conn.putrequest("GET", f"/documents/{doc_id}/download", skip_accept_encoding=True)
            for key, value in headers.items():
                conn.putheader(key, value)
            conn.putheader("Connection", "close")
            conn.endheaders()
            response = conn.getresponse()
            if not 200 <= response.status < 300:
                body = response.read().decode("utf-8", "replace")
                raise RuntimeError(f"download {doc_id} failed with HTTP {response.status}: {body}")
            total = 0
            while chunk := response.read(DEFAULT_BODY_BLOCK_BYTES):
                total += len(chunk)
            return total
        finally:
            conn.close()

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, case.users)) as executor:
        downloaded_sizes = list(
            executor.map(lambda item: download_doc(item[0], item[1]), doc_ids),
        )
    download_wall = time.perf_counter() - download_started
    expected_download = case.users * file_size
    if sum(downloaded_sizes) != expected_download:
        raise AssertionError("download size mismatch")

    part_wall = max(part_finished_at) - min(part_started_at)
    complete_wall = max(complete_finished_at) - min(complete_started_at)
    return CaseResult(
        name=case.name,
        mode="app",
        server="rust",
        users=case.users,
        file_mib=case.file_mib,
        workers=case.workers,
        chunk_mib=case.chunk_mib,
        upload_wall_seconds=upload_wall,
        upload_mib_per_second=total_mib / upload_wall,
        part_wall_seconds=part_wall,
        part_mib_per_second=total_mib / part_wall,
        complete_wall_seconds=complete_wall,
        download_wall_seconds=download_wall,
        download_mib_per_second=total_mib / download_wall,
        part_count=len(part_durations),
        part_min_seconds=min(part_durations),
        part_p50_seconds=percentile(part_durations, 0.50),
        part_p95_seconds=percentile(part_durations, 0.95),
        part_max_seconds=max(part_durations),
        server_cpu_seconds=None,
        server_rss_mib=None,
        server_peak_rss_mib=None,
    )


def run_case(
    mode: str,
    case: BenchCase,
    body_block: bytes,
    *,
    part_checksum: bool,
    client_rate_mib: float | None,
    sink_checksum: bool,
    sink_write: bool,
    rust_bin: Path | None,
    runner: str,
    docker_image: str,
    startup_timeout: float,
) -> CaseResult:
    with tempfile.TemporaryDirectory(prefix="vault-transfer-bench-") as tmp:
        temp_dir = Path(tmp)
        if runner == "docker":
            temp_dir.chmod(0o777)
        port = free_port()
        proc = start_server(
            mode=mode,
            runner=runner,
            port=port,
            temp_dir=temp_dir,
            chunk_mib=case.chunk_mib,
            sink_checksum=sink_checksum,
            sink_write=sink_write,
            rust_bin=rust_bin,
            docker_image=docker_image,
        )
        try:
            wait_health("127.0.0.1", port, proc, startup_timeout)
            before_usage = process_usage(proc.pid)
            if mode == RUST_SINK_MODE:
                result = benchmark_sink_case(
                    host="127.0.0.1",
                    port=port,
                    case=case,
                    body_block=body_block,
                    client_rate_mib=client_rate_mib,
                    sink_checksum=sink_checksum,
                )
            else:
                result = benchmark_app_case(
                    host="127.0.0.1",
                    port=port,
                    case=case,
                    body_block=body_block,
                    part_checksum=part_checksum,
                    client_rate_mib=client_rate_mib,
                )
            return with_process_usage(result, before_usage, process_usage(proc.pid))
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)


def print_result(result: CaseResult) -> None:
    download = (
        "download=n/a"
        if result.download_mib_per_second is None or result.download_wall_seconds is None
        else (
            f"download={result.download_mib_per_second:.1f}MiB/s "
            f"({result.download_wall_seconds:.3f}s)"
        )
    )
    print(
        f"{result.mode}:{result.name} users={result.users} file={result.file_mib}MiB "
        f"server={result.server} workers={result.workers} chunk={result.chunk_mib or 'server'}MiB "
        f"upload={result.upload_mib_per_second:.1f}MiB/s ({result.upload_wall_seconds:.3f}s), "
        f"parts={result.part_mib_per_second or 0.0:.1f}MiB/s "
        f"({result.part_wall_seconds or 0.0:.3f}s), "
        f"complete={result.complete_wall_seconds or 0.0:.3f}s, "
        f"{download}, parts={result.part_count}, "
        f"part_s=min {result.part_min_seconds:.3f} p50 {result.part_p50_seconds:.3f} "
        f"p95 {result.part_p95_seconds:.3f} max {result.part_max_seconds:.3f}, "
        f"server_cpu={result.server_cpu_seconds or 0.0:.3f}s "
        f"rss={result.server_rss_mib or 0.0:.1f}MiB "
        f"peak_rss={result.server_peak_rss_mib or 0.0:.1f}MiB",
        flush=True,
    )


def run() -> dict[str, Any]:
    args = parse_args()
    if args.runner == "docker" and args.mode != "app":
        raise SystemExit("--runner=docker currently supports only --mode=app")
    if args.runner == "docker" and args.docker_build:
        build_docker_image(args.docker_image)
    cases_by_name = default_cases()
    selected = [cases_by_name[name] for name in (args.case or cases_by_name)]
    if args.workers is not None:
        selected = [replace(case, workers=max(1, args.workers)) for case in selected]
    modes = ("app", RUST_SINK_MODE) if args.mode == "both" else (args.mode,)
    body_block = b"x" * max(1, args.body_block_kib * 1024)
    results: list[CaseResult] = []
    thresholds = configured_thresholds(args)
    for mode in modes:
        for case in selected:
            result = run_case(
                mode,
                case,
                body_block,
                part_checksum=args.part_checksum,
                client_rate_mib=args.client_rate_mib,
                sink_checksum=not args.sink_no_checksum,
                sink_write=args.sink_write,
                rust_bin=args.rust_bin,
                runner=args.runner,
                docker_image=args.docker_image,
                startup_timeout=args.startup_timeout,
            )
            results.append(result)
            if not args.quiet:
                print_result(result)
    failures = threshold_failures(results, thresholds)
    payload = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "body_block_kib": args.body_block_kib,
        "client_rate_mib": args.client_rate_mib,
        "part_checksum": args.part_checksum,
        "app_server": "rust",
        "runner": args.runner,
        "docker_image": args.docker_image if args.runner == "docker" else None,
        "sink_checksum": not args.sink_no_checksum,
        "sink_write": args.sink_write,
        "thresholds": [asdict(threshold) for threshold in thresholds],
        "threshold_failures": failures,
        "results": [asdict(result) for result in results],
    }
    if args.json:
        args.json.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if args.quiet:
        print(json.dumps(payload, indent=2, sort_keys=True))
    if failures:
        for failure in failures:
            print(f"benchmark threshold failed: {failure}", file=sys.stderr)
        raise SystemExit(1)
    return payload


if __name__ == "__main__":
    run()
