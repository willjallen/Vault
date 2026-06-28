import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import bench_transfers


class TransferBenchmarkHarnessTests(unittest.TestCase):
    def case_result(
        self,
        *,
        name: str = "single-128m",
        mode: str = "app",
        server: str = "rust",
        upload_mib_per_second: float = 500.0,
        download_mib_per_second: float | None = 1_000.0,
    ) -> bench_transfers.CaseResult:
        return bench_transfers.CaseResult(
            name=name,
            mode=mode,
            server=server,
            users=1,
            file_mib=128,
            workers=16,
            chunk_mib=None,
            upload_wall_seconds=1.0,
            upload_mib_per_second=upload_mib_per_second,
            part_wall_seconds=1.0,
            part_mib_per_second=upload_mib_per_second,
            complete_wall_seconds=0.1,
            download_wall_seconds=1.0 if download_mib_per_second is not None else None,
            download_mib_per_second=download_mib_per_second,
            part_count=4,
            part_min_seconds=0.1,
            part_p50_seconds=0.2,
            part_p95_seconds=0.3,
            part_max_seconds=0.4,
            server_cpu_seconds=None,
            server_rss_mib=None,
            server_peak_rss_mib=None,
        )

    def test_server_env_configures_rust_runtime_paths_and_bind_port(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-bench-test-") as tmp:
            temp_dir = Path(tmp)
            env = bench_transfers.server_env(temp_dir, chunk_mib=4, port=19001)

            self.assertEqual(env["VAULT_HOST"], "127.0.0.1")
            self.assertEqual(env["VAULT_PORT"], "19001")
            self.assertEqual(env["VAULT_DATA_DIR"], str(temp_dir))
            self.assertEqual(env["VAULT_DB_PATH"], str(temp_dir / "vault.db"))
            self.assertEqual(env["VAULT_OBJECTS_PATH"], str(temp_dir / "objects"))
            self.assertEqual(env["VAULT_TRANSFERS_PATH"], str(temp_dir / "transfers"))
            self.assertEqual(env["VAULT_TRANSFER_CHUNK_BYTES"], str(4 * bench_transfers.MIB))
            self.assertEqual(env["VAULT_AUTH_MODE"], "headers")
            self.assertEqual(env["VAULT_SESSION_SECRET"], "benchmark-session-secret")
            self.assertEqual(env["VAULT_BENCH_SINK_DIR"], str(temp_dir / "sink"))

    def test_rust_server_command_uses_explicit_binary_or_cargo_release_fallback(self) -> None:
        self.assertEqual(
            bench_transfers.rust_server_command(Path("/opt/vault-server")),
            ["/opt/vault-server"],
        )

        fallback = bench_transfers.rust_server_command(None)
        binary_name = "vault-server.exe" if sys.platform == "win32" else "vault-server"

        self.assertIn(
            fallback,
            [
                [str(Path("target") / "release" / binary_name)],
                ["cargo", "run", "--release", "-p", "vault-server", "--"],
            ],
        )

    def test_uvicorn_command_preserves_python_benchmark_server_flags(self) -> None:
        command = bench_transfers.uvicorn_command("app.main:app", 19002)

        self.assertEqual(command[:4], [sys.executable, "-m", "uvicorn", "app.main:app"])
        self.assertIn("--loop", command)
        self.assertIn("uvloop", command)
        self.assertIn("--http", command)
        self.assertIn("httptools", command)
        self.assertIn("--no-access-log", command)

    def test_start_server_selects_rust_app_command_and_python_sink_command(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-bench-test-") as tmp:
            with patch("scripts.bench_transfers.subprocess.Popen") as popen:
                bench_transfers.start_server(
                    mode="app",
                    server="rust",
                    runner="direct",
                    port=19003,
                    temp_dir=Path(tmp),
                    chunk_mib=None,
                    sink_checksum=True,
                    sink_write=True,
                    rust_bin=Path("/opt/vault-server"),
                    docker_image="vault-bench:local",
                )

            command = popen.call_args.args[0]
            env = popen.call_args.kwargs["env"]
            self.assertEqual(command, ["/opt/vault-server"])
            self.assertEqual(env["VAULT_PORT"], "19003")
            self.assertEqual(env["VAULT_BENCH_SINK_HASH"], "1")
            self.assertEqual(env["VAULT_BENCH_SINK_WRITE"], "1")
            self.assertEqual(env["VAULT_BENCH_ROUTES"], "0")

            with patch("scripts.bench_transfers.subprocess.Popen") as sink_popen:
                bench_transfers.start_server(
                    mode="sink",
                    server="rust",
                    runner="direct",
                    port=19004,
                    temp_dir=Path(tmp),
                    chunk_mib=None,
                    sink_checksum=False,
                    sink_write=False,
                    rust_bin=Path("/opt/vault-server"),
                    docker_image="vault-bench:local",
                )

            sink_command = sink_popen.call_args.args[0]
            sink_env = sink_popen.call_args.kwargs["env"]
            self.assertIn("scripts.bench_transfers:sink_app", sink_command)
            self.assertEqual(sink_env["VAULT_BENCH_SINK_HASH"], "0")
            self.assertEqual(sink_env["VAULT_BENCH_SINK_WRITE"], "0")
            self.assertEqual(sink_env["VAULT_BENCH_ROUTES"], "0")

    def test_start_server_selects_rust_sink_route_with_benchmark_routes_enabled(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-bench-test-") as tmp:
            with patch("scripts.bench_transfers.subprocess.Popen") as popen:
                bench_transfers.start_server(
                    mode="rust-sink",
                    server="rust",
                    runner="direct",
                    port=19007,
                    temp_dir=Path(tmp),
                    chunk_mib=None,
                    sink_checksum=False,
                    sink_write=True,
                    rust_bin=Path("/opt/vault-server"),
                    docker_image="vault-bench:local",
                )

        command = popen.call_args.args[0]
        env = popen.call_args.kwargs["env"]
        self.assertEqual(command, ["/opt/vault-server"])
        self.assertEqual(env["VAULT_BENCH_ROUTES"], "1")
        self.assertEqual(env["VAULT_BENCH_SINK_HASH"], "0")
        self.assertEqual(env["VAULT_BENCH_SINK_WRITE"], "1")

    def test_docker_server_command_uses_container_runtime_paths_and_scoped_env(self) -> None:
        data_dir = Path("vault-bench-data").resolve()
        command = bench_transfers.docker_server_command(
            image="vault-bench:test",
            host_port=19005,
            data_dir=data_dir,
            chunk_mib=4,
        )

        self.assertEqual(
            command[:5],
            ["docker", "run", "--rm", "--publish", "127.0.0.1:19005:8000"],
        )
        self.assertIn("--volume", command)
        self.assertIn(f"{data_dir}:/data", command)
        self.assertEqual(command[-1], "vault-bench:test")
        env_values = [command[index + 1] for index, value in enumerate(command) if value == "--env"]
        self.assertIn("VAULT_HOST=0.0.0.0", env_values)
        self.assertIn("VAULT_PORT=8000", env_values)
        self.assertIn("VAULT_DATA_DIR=/data", env_values)
        self.assertIn("VAULT_DB_PATH=/data/vault.db", env_values)
        self.assertIn("VAULT_OBJECTS_PATH=/data/objects", env_values)
        self.assertIn("VAULT_TRANSFERS_PATH=/data/transfers", env_values)
        self.assertIn("VAULT_STATIC_DIR=/app/app/static", env_values)
        self.assertIn("VAULT_TRANSFER_CHUNK_BYTES=4194304", env_values)
        self.assertIn("VAULT_DOCKER_RUNTIME=1", env_values)

    def test_start_server_selects_docker_runner_command(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-bench-test-") as tmp:
            with patch("scripts.bench_transfers.subprocess.Popen") as popen:
                bench_transfers.start_server(
                    mode="app",
                    server="rust",
                    runner="docker",
                    port=19006,
                    temp_dir=Path(tmp),
                    chunk_mib=None,
                    sink_checksum=True,
                    sink_write=True,
                    rust_bin=Path("/opt/vault-server"),
                    docker_image="vault-bench:test",
                )

        command = popen.call_args.args[0]
        self.assertEqual(
            command[:5],
            ["docker", "run", "--rm", "--publish", "127.0.0.1:19006:8000"],
        )
        self.assertEqual(command[-1], "vault-bench:test")

    def test_local_direct_thresholds_match_documented_targets(self) -> None:
        thresholds = {
            threshold.case_name: threshold
            for threshold in bench_transfers.local_direct_thresholds()
        }

        self.assertEqual(thresholds["single-128m"].min_upload_mib_per_second, 400.0)
        self.assertEqual(thresholds["ten-64m"].min_upload_mib_per_second, 500.0)
        self.assertEqual(
            thresholds["ten-64m-4m-parts"].min_upload_mib_per_second,
            450.0,
        )
        for threshold in thresholds.values():
            self.assertEqual(threshold.mode, "app")
            self.assertEqual(threshold.server, "rust")
            self.assertEqual(threshold.min_download_mib_per_second, 900.0)

    def test_threshold_failures_report_upload_download_and_missing_downloads(self) -> None:
        thresholds = [
            bench_transfers.ThroughputThreshold(
                case_name="single-128m",
                mode="app",
                server="rust",
                min_upload_mib_per_second=400.0,
                min_download_mib_per_second=900.0,
            ),
        ]

        failures = bench_transfers.threshold_failures(
            [
                self.case_result(
                    upload_mib_per_second=399.9,
                    download_mib_per_second=899.9,
                ),
                self.case_result(
                    name="single-128m",
                    upload_mib_per_second=450.0,
                    download_mib_per_second=None,
                ),
            ],
            thresholds,
        )

        self.assertIn("app:single-128m upload 399.9MiB/s below 400.0MiB/s", failures)
        self.assertIn(
            "app:single-128m download 899.9MiB/s below 900.0MiB/s",
            failures,
        )
        self.assertIn(
            "app:single-128m download missing below 900.0MiB/s",
            failures,
        )

    def test_thresholds_ignore_non_matching_cases_modes_and_servers(self) -> None:
        threshold = bench_transfers.ThroughputThreshold(
            case_name="single-128m",
            mode="app",
            server="rust",
            min_upload_mib_per_second=400.0,
            min_download_mib_per_second=900.0,
        )

        failures = bench_transfers.threshold_failures(
            [
                self.case_result(name="kevin-38m", upload_mib_per_second=1.0),
                self.case_result(mode="sink", server="asgi-sink", upload_mib_per_second=1.0),
                self.case_result(server="python", upload_mib_per_second=1.0),
            ],
            [threshold],
        )

        self.assertEqual(failures, [])

    def test_status_value_mib_parses_linux_process_status_units(self) -> None:
        status = "\n".join(
            [
                "Name:\tvault-server",
                "VmRSS:\t2048 kB",
                "VmHWM:\t3 MB",
                "VmData:\t1 GB",
                "VmBad:\tnot-a-number kB",
            ],
        )

        self.assertEqual(bench_transfers.status_value_mib(status, "VmRSS"), 2.0)
        self.assertEqual(bench_transfers.status_value_mib(status, "VmHWM"), 3.0)
        self.assertEqual(bench_transfers.status_value_mib(status, "VmData"), 1024.0)
        self.assertIsNone(bench_transfers.status_value_mib(status, "VmBad"))
        self.assertIsNone(bench_transfers.status_value_mib(status, "VmMissing"))

    def test_with_process_usage_adds_cpu_delta_and_memory_snapshot(self) -> None:
        result = bench_transfers.with_process_usage(
            self.case_result(),
            bench_transfers.ProcessUsage(
                cpu_seconds=1.25,
                rss_mib=10.0,
                peak_rss_mib=12.0,
            ),
            bench_transfers.ProcessUsage(
                cpu_seconds=3.75,
                rss_mib=20.0,
                peak_rss_mib=24.0,
            ),
        )

        self.assertEqual(result.server_cpu_seconds, 2.5)
        self.assertEqual(result.server_rss_mib, 20.0)
        self.assertEqual(result.server_peak_rss_mib, 24.0)

    def test_with_process_usage_leaves_metrics_empty_when_unavailable(self) -> None:
        result = bench_transfers.with_process_usage(self.case_result(), None, None)

        self.assertIsNone(result.server_cpu_seconds)
        self.assertIsNone(result.server_rss_mib)
        self.assertIsNone(result.server_peak_rss_mib)

    def test_open_sink_file_uses_configured_sink_directory(self) -> None:
        with tempfile.TemporaryDirectory(prefix="vault-bench-sink-") as tmp:
            sink_dir = Path(tmp) / "sink"
            with patch.dict(
                "scripts.bench_transfers.os.environ", {"VAULT_BENCH_SINK_DIR": str(sink_dir)}
            ):
                sink_file = bench_transfers.open_sink_file()
                sink_file.write(b"payload")
                sink_file.close()

            files = list(sink_dir.iterdir())
            self.assertEqual(len(files), 1)
            self.assertEqual(files[0].read_bytes(), b"payload")


if __name__ == "__main__":
    unittest.main()
