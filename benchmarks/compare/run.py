#!/usr/bin/env python3
"""Reproducible Lavalink 4.2.2 versus lavalink-rs release gate."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import random
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import wave
import zipfile
from pathlib import Path


UPSTREAM_VERSION = "4.2.2"
UPSTREAM_URL = "https://github.com/lavalink-devs/Lavalink/releases/download/4.2.2/Lavalink.jar"
UPSTREAM_SHA256 = "8cb801e591072c3689fafd71ccf571a95a4ead3cc35dfc045e157d763d89119a"
PASSWORD = "benchmark"
TRACK_MS = 60_000
ROOT = Path(__file__).resolve().parents[2]
WORK = ROOT / "target" / "compare"
JAR = WORK / f"Lavalink-{UPSTREAM_VERSION}.jar"
EXTRACTED = WORK / "upstream"
JAVA_CLASSES = WORK / "java-classes"
RUST_COMPARE = ROOT / "target" / "release" / "examples" / ("compare.exe" if os.name == "nt" else "compare")
FIXTURES = WORK / "fixtures"
LOGS = WORK / "logs"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def run_checked(command: list[str], **kwargs) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(map(str, command)), flush=True)
    return subprocess.run(command, check=True, text=True, **kwargs)


def download_upstream() -> None:
    WORK.mkdir(parents=True, exist_ok=True)
    if JAR.exists() and sha256(JAR) == UPSTREAM_SHA256:
        return
    partial = JAR.with_suffix(".part")
    partial.unlink(missing_ok=True)
    print(f"downloading {UPSTREAM_URL}", flush=True)
    urllib.request.urlretrieve(UPSTREAM_URL, partial)
    actual = sha256(partial)
    if actual != UPSTREAM_SHA256:
        partial.unlink(missing_ok=True)
        raise RuntimeError(f"upstream JAR SHA-256 mismatch: {actual}")
    partial.replace(JAR)


def java_major() -> int:
    output = subprocess.run(["java", "-version"], capture_output=True, text=True, check=True)
    first = (output.stderr or output.stdout).splitlines()[0]
    version = first.split('"')[1]
    return int(version.split(".")[1] if version.startswith("1.") else version.split(".")[0])


def prepare_java() -> None:
    download_upstream()
    marker = EXTRACTED / ".sha256"
    if not marker.exists() or marker.read_text().strip() != UPSTREAM_SHA256:
        if EXTRACTED.exists():
            shutil.rmtree(EXTRACTED)
        EXTRACTED.mkdir(parents=True)
        with zipfile.ZipFile(JAR) as archive:
            archive.extractall(EXTRACTED)
        marker.write_text(UPSTREAM_SHA256 + "\n")
    JAVA_CLASSES.mkdir(parents=True, exist_ok=True)
    classpath = os.pathsep.join([
        str(EXTRACTED / "BOOT-INF" / "classes"),
        str(EXTRACTED / "BOOT-INF" / "lib" / "*"),
    ])
    run_checked([
        "javac", "-cp", classpath, "-d", str(JAVA_CLASSES),
        str(ROOT / "benchmarks" / "compare" / "AudioBench.java"),
    ])


def write_wav(path: Path, seconds: int = 60) -> None:
    sample_rate = 44_100
    with wave.open(str(path), "wb") as output:
        output.setnchannels(2)
        output.setsampwidth(2)
        output.setframerate(sample_rate)
        for second in range(seconds):
            samples = bytearray()
            for offset in range(sample_rate):
                frame = second * sample_rate + offset
                t = frame / sample_rate
                value = (
                    math.sin(math.tau * 220.0 * t) * 0.32
                    + math.sin(math.tau * 440.0 * t) * 0.18
                    + math.sin(math.tau * 1760.0 * t) * 0.08
                )
                left = max(-32768, min(32767, round(value * 32767)))
                right = max(-32768, min(32767, round(value * 0.93 * 32767)))
                samples.extend(struct.pack("<hh", left, right))
            output.writeframesraw(samples)


def prepare_fixtures() -> dict:
    if shutil.which("ffmpeg") is None:
        raise RuntimeError("ffmpeg is required to create the FLAC and AAC comparison fixtures")
    FIXTURES.mkdir(parents=True, exist_ok=True)
    wav = FIXTURES / "fixture.wav"
    flac = FIXTURES / "fixture.flac"
    m4a = FIXTURES / "fixture.m4a"
    write_wav(wav)
    run_checked(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", str(wav), "-c:a", "flac", str(flac)])
    run_checked(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", str(wav), "-c:a", "aac", "-b:a", "192k", str(m4a)])
    ffmpeg_version = subprocess.run(["ffmpeg", "-version"], capture_output=True, text=True, check=True).stdout.splitlines()[0]
    manifest = {
        "duration_seconds": 60,
        "sample_rate": 44_100,
        "channels": 2,
        "ffmpeg": ffmpeg_version,
        "files": {path.suffix[1:]: {"path": str(path), "sha256": sha256(path)} for path in [wav, flac, m4a]},
    }
    (FIXTURES / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def prepare() -> None:
    if java_major() < 17:
        raise RuntimeError("JDK 17 or newer is required by Lavalink 4.2.2")
    prepare_java()
    prepare_fixtures()
    run_checked([
        "cargo", "build", "-p", "lavalink-server", "--release", "--locked",
        "--bin", "lavalink-server", "--example", "compare",
    ], cwd=ROOT)


def parse_cpu_set(value: str) -> list[int]:
    cpus: set[int] = set()
    for part in value.split(","):
        bounds = part.strip().split("-", 1)
        if len(bounds) == 1:
            cpus.add(int(bounds[0]))
        else:
            start, end = map(int, bounds)
            if end < start:
                raise ValueError(f"invalid CPU range: {part}")
            cpus.update(range(start, end + 1))
    if not cpus:
        raise ValueError("CPU set cannot be empty")
    return sorted(cpus)


def taskset(cpus: str, command: list[str]) -> list[str]:
    return ["taskset", "-c", cpus, *map(str, command)]


def proc_sample(pid: int) -> tuple[int, int]:
    fields = Path(f"/proc/{pid}/stat").read_text().split()
    ticks = int(fields[13]) + int(fields[14])
    rss = 0
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1])
            break
    return ticks, rss


def invalid_measurement(return_code: int, marker_seen: bool, payload: dict | None) -> bool:
    return return_code != 0 or not marker_seen or payload is None


def run_measured(
    command: list[str],
    affinity: str,
    observed_pid: int | None = None,
    cwd: Path = ROOT,
) -> dict:
    LOGS.mkdir(parents=True, exist_ok=True)
    log_path = LOGS / f"workload-{time.time_ns()}.log"
    with log_path.open("w", encoding="utf-8") as error_log:
        process = subprocess.Popen(
            taskset(affinity, command), cwd=cwd, text=True,
            stdout=subprocess.PIPE, stderr=error_log, bufsize=1,
        )
        watched = observed_pid or process.pid
        measuring = threading.Event()
        stop_monitor = threading.Event()
        rss_samples: list[int] = []

        def sample_rss() -> None:
            while not stop_monitor.wait(0.1):
                if not measuring.is_set():
                    continue
                try:
                    rss_samples.append(proc_sample(watched)[1])
                except (FileNotFoundError, ProcessLookupError):
                    return

        monitor = threading.Thread(target=sample_rss, daemon=True)
        monitor.start()
        marker_seen = False
        observed_start = child_start = None
        payload = None
        assert process.stdout is not None
        for raw in process.stdout:
            line = raw.strip()
            if line == "BENCHMARK_MEASURE":
                observed_start = proc_sample(watched)[0]
                child_start = proc_sample(process.pid)[0]
                measuring.set()
                marker_seen = True
            elif line.startswith("{") and '"workload"' in line:
                payload = json.loads(line)
        # stdout reaches EOF after exit but before wait() reaps the child, so its
        # final /proc counters are still available here.
        child_end = proc_sample(process.pid)[0] if Path(f"/proc/{process.pid}").exists() else None
        observed_end = proc_sample(watched)[0] if observed_pid is not None else child_end
        return_code = process.wait()
        stop_monitor.set()
        monitor.join(timeout=2)
    if invalid_measurement(return_code, marker_seen, payload):
        tail = log_path.read_text(errors="replace")[-4_000:]
        raise RuntimeError(f"workload failed ({return_code}); log={log_path}\n{tail}")

    ticks_per_second = os.sysconf("SC_CLK_TCK")
    payload["cpu_seconds"] = (
        (observed_end - observed_start) / ticks_per_second
        if observed_start is not None and observed_end is not None else None
    )
    if child_start is not None and child_end is not None:
        payload["driver_cpu_seconds"] = (child_end - child_start) / ticks_per_second
    payload["peak_rss_kb"] = max(rss_samples, default=0)
    payload["steady_rss_kb"] = statistics.median(rss_samples) if rss_samples else 0
    payload["rss_kb_samples"] = rss_samples
    payload["log"] = str(log_path.relative_to(ROOT))
    payload["command"] = command
    payload["cpu_affinity"] = affinity
    return payload


def ensure_linux(args: argparse.Namespace) -> None:
    if platform.system() != "Linux":
        raise RuntimeError("official comparison runs are Linux-only; use --self-test on other hosts")
    if shutil.which("taskset") is None:
        raise RuntimeError("taskset is required")
    server = set(parse_cpu_set(args.server_cpus))
    driver = set(parse_cpu_set(args.driver_cpus))
    if server & driver:
        raise RuntimeError("server and driver CPU sets overlap")
    if len(server) < 4 or len(driver) < 2:
        raise RuntimeError("use at least four server cores and two isolated driver cores")
    physical = set()
    for cpu in server | driver:
        topology = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
        package = (topology / "physical_package_id").read_text().strip()
        core = (topology / "core_id").read_text().strip()
        physical.add((package, core))
        siblings_path = topology / "thread_siblings_list"
        if siblings_path.exists():
            siblings = set(parse_cpu_set(siblings_path.read_text().strip()))
            if siblings & server and siblings & driver:
                raise RuntimeError(f"SMT siblings are split between server and driver sets: {sorted(siblings)}")
    if len(physical) < 6:
        raise RuntimeError("the official gate requires at least six physical cores")
    if not args.allow_unstable_host:
        swaps = Path("/proc/swaps").read_text().splitlines()[1:]
        if swaps:
            raise RuntimeError("swap is active; disable it or use --allow-unstable-host for a diagnostic run")
        governors = []
        for cpu in server | driver:
            path = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor")
            if path.exists():
                governors.append(path.read_text().strip())
        if governors and any(governor != "performance" for governor in governors):
            raise RuntimeError("all assigned CPUs must use the performance governor; use --allow-unstable-host only for diagnostics")


def ensure_artifacts(args: argparse.Namespace) -> dict:
    required = [
        JAR, EXTRACTED / "BOOT-INF" / "classes", JAVA_CLASSES / "AudioBench.class",
        RUST_COMPARE, FIXTURES / "manifest.json", FIXTURES / "fixture.wav",
        FIXTURES / "fixture.flac", FIXTURES / "fixture.m4a",
    ]
    missing = [str(path) for path in required if not path.exists()]
    if missing:
        raise RuntimeError("run `python3 benchmarks/compare/run.py prepare` first; missing: " + ", ".join(missing))
    if not Path(args.rust_bin).exists():
        raise RuntimeError(f"Rust server binary does not exist: {args.rust_bin}")
    if sha256(JAR) != UPSTREAM_SHA256:
        raise RuntimeError("prepared upstream JAR no longer matches the pinned SHA-256")
    return json.loads((FIXTURES / "manifest.json").read_text())


def java_classpath() -> str:
    return os.pathsep.join([
        str(JAVA_CLASSES),
        str(EXTRACTED / "BOOT-INF" / "classes"),
        str(EXTRACTED / "BOOT-INF" / "lib" / "*"),
    ])


def implementation_order(trial: int) -> list[str]:
    return ["rust", "java"] if trial % 2 == 0 else ["java", "rust"]


def audio_command(
    implementation: str,
    input_path: Path,
    filter_name: str,
    mode: str,
    concurrency: int,
    warmup: int,
    measure: int,
    server_cores: int,
) -> list[str]:
    common = [
        "--input", str(input_path), "--filter", filter_name, "--mode", mode,
        "--concurrency", str(concurrency), "--warmup-seconds", str(warmup),
        "--measure-seconds", str(measure),
    ]
    if implementation == "rust":
        return [str(RUST_COMPARE), "audio", *common, "--track-ms", str(TRACK_MS)]
    return [
        "java", f"-XX:ActiveProcessorCount={server_cores}", "-cp", java_classpath(),
        "AudioBench", *common,
    ]


def finish_audio_result(result: dict, trial: int, input_path: Path) -> dict:
    result["trial"] = trial
    result["input"] = input_path.suffix[1:]
    result["input_sha256"] = sha256(input_path)
    if result.get("cpu_seconds"):
        result["audio_seconds_per_cpu_second"] = result["frames"] * 0.02 / result["cpu_seconds"]
    return result


def run_audio_case(
    args: argparse.Namespace,
    implementation: str,
    input_path: Path,
    filter_name: str,
    mode: str,
    concurrency: int,
    trial: int,
    measure: int,
) -> dict:
    command = audio_command(
        implementation, input_path, filter_name, mode, concurrency,
        args.warmup_seconds, measure, len(parse_cpu_set(args.server_cpus)),
    )
    result = run_measured(command, args.server_cpus)
    # Direct audio processes are reaped before /proc can be sampled at exit on
    # some kernels. A missing CPU value invalidates CPU-efficiency only; wall,
    # deadline and RSS evidence remain usable and are never fabricated.
    return finish_audio_result(result, trial, input_path)


def stable(result: dict) -> bool:
    attempts = result["frames"] + result["misses"]
    miss_rate = result["misses"] / attempts if attempts else 1.0
    result["miss_rate"] = miss_rate
    return miss_rate <= 0.001 and result["p99_service_us"] <= 20_000


def discover_capacity(args: argparse.Namespace, implementation: str, filter_name: str, input_path: Path) -> tuple[int, bool, list[dict]]:
    cores = len(parse_cpu_set(args.server_cpus))
    maximum = cores * args.capacity_max_multiplier
    low = 0
    candidate = cores
    evidence: list[dict] = []
    while candidate <= maximum:
        result = run_audio_case(args, implementation, input_path, filter_name, "realtime", candidate, 0, args.discovery_seconds)
        result["phase"] = "capacity-discovery"
        evidence.append(result)
        if stable(result):
            low = candidate
            if candidate == maximum:
                return low, True, evidence
            candidate = min(maximum, candidate * 2)
            if candidate == low:
                return low, True, evidence
        else:
            break
    high = candidate
    while high - low > 1:
        candidate = (low + high) // 2
        result = run_audio_case(args, implementation, input_path, filter_name, "realtime", candidate, 0, args.discovery_seconds)
        result["phase"] = "capacity-discovery"
        evidence.append(result)
        if stable(result):
            low = candidate
        else:
            high = candidate
    return low, False, evidence


def audio_suite(args: argparse.Namespace, manifest: dict) -> tuple[list[dict], dict]:
    files = {extension: Path(entry["path"]) for extension, entry in manifest["files"].items()}
    cores = len(parse_cpu_set(args.server_cpus))
    cases = [(files["wav"], "default"), (files["flac"], "default"), (files["m4a"], "default"), (files["m4a"], "eq"), (files["m4a"], "timescale")]
    results: list[dict] = []
    for trial in range(args.trials):
        for implementation in implementation_order(trial):
            trial_cases = cases if trial % 2 == 0 else list(reversed(cases))
            concurrencies = [1, cores, cores * 2]
            if trial % 2:
                concurrencies.reverse()
            for input_path, filter_name in trial_cases:
                for concurrency in concurrencies:
                    result = run_audio_case(args, implementation, input_path, filter_name, "throughput", concurrency, trial, args.audio_measure_seconds)
                    result["phase"] = "throughput"
                    results.append(result)

    capacity: dict[str, dict] = {}
    for filter_name in ["default", "eq"]:
        capacity[filter_name] = {}
        for implementation in ["rust", "java"]:
            found, capped, discovery = discover_capacity(args, implementation, filter_name, files["m4a"])
            results.extend(discovery)
            capacity[filter_name][implementation] = {
                "players": found,
                "capped": capped,
                "verified": False,
            }
        verified: dict[str, list[dict]] = {"rust": [], "java": []}
        for trial in range(args.trials):
            for implementation in implementation_order(trial):
                found = capacity[filter_name][implementation]["players"]
                if found == 0:
                    continue
                result = run_audio_case(args, implementation, files["m4a"], filter_name, "realtime", found, trial, args.realtime_seconds)
                result["phase"] = "capacity-verification"
                stable(result)
                results.append(result)
                verified[implementation].append(result)
        for implementation in ["rust", "java"]:
            values = verified[implementation]
            capacity[filter_name][implementation]["verified"] = bool(values) and all(stable(result) for result in values)
    return results, capacity


def config_text(port: int) -> str:
    return f"""server:
  port: {port}
  address: 127.0.0.1
lavalink:
  server:
    password: {PASSWORD}
    sources:
      youtube: false
      soundcloud: false
      bandcamp: false
      twitch: false
      vimeo: false
      nico: false
      http: false
      local: true
    frameBufferDurationMs: 5000
    nonAllocatingFrameBuffer: false
    opusEncodingQuality: 10
    resamplingQuality: LOW
metrics:
  prometheus:
    enabled: false
logging:
  level:
    root: WARN
  request:
    enabled: false
"""


def server_command(args: argparse.Namespace, implementation: str, config: Path) -> tuple[list[str], Path]:
    if implementation == "rust":
        return [str(Path(args.rust_bin).resolve()), str(config)], ROOT
    cores = len(parse_cpu_set(args.server_cpus))
    return [
        "java", f"-XX:ActiveProcessorCount={cores}", "-jar", str(JAR),
        f"--spring.config.location={config.as_uri()}", "--logging.level.root=WARN",
        "--spring.cloud.config.enabled=false",
    ], WORK


def readiness(port: int, started: float, timeout: float = 120.0) -> float:
    request = urllib.request.Request(f"http://127.0.0.1:{port}/v4/info", headers={"Authorization": PASSWORD})
    while time.monotonic() - started < timeout:
        try:
            with urllib.request.urlopen(request, timeout=1) as response:
                if response.status == 200:
                    return time.monotonic() - started
        except Exception:
            time.sleep(0.1)
    raise RuntimeError("server did not become ready")


def launch_server(args: argparse.Namespace, implementation: str, config: Path) -> tuple[subprocess.Popen, object, float]:
    command, cwd = server_command(args, implementation, config)
    LOGS.mkdir(parents=True, exist_ok=True)
    log = (LOGS / f"{implementation}-server-{time.time_ns()}.log").open("w", encoding="utf-8")
    started = time.monotonic()
    process = subprocess.Popen(taskset(args.server_cpus, command), cwd=cwd, stdout=log, stderr=subprocess.STDOUT, text=True)
    try:
        ready = readiness(args.port, started)
    except Exception:
        process.terminate()
        process.wait(timeout=10)
        log.close()
        raise
    return process, log, ready


def stop_server(process: subprocess.Popen, log) -> None:
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
    log.close()


def encoded_track_token() -> str:
    completed = run_checked([
        str(RUST_COMPARE), "encoded-track", "--input", str(FIXTURES / "fixture.wav"),
        "--track-ms", str(TRACK_MS),
    ], capture_output=True)
    return completed.stdout.strip().splitlines()[-1]


def http_suite(args: argparse.Namespace) -> tuple[list[dict], dict]:
    config = WORK / "application-benchmark.yml"
    config.write_text(config_text(args.port))
    token = encoded_track_token()
    cores = len(parse_cpu_set(args.server_cpus))
    results: list[dict] = []
    startup: dict[str, list[float]] = {"rust": [], "java": []}

    for implementation in ["rust", "java"]:
        for attempt in range(5):
            process, log, ready = launch_server(args, implementation, config)
            stop_server(process, log)
            if attempt > 0:
                startup[implementation].append(ready)

    for trial in range(args.trials):
        for implementation in implementation_order(trial):
            process, log, _ready = launch_server(args, implementation, config)
            try:
                idle_rss = proc_sample(process.pid)[1]
                concurrencies = [1, cores * 4]
                if trial % 2:
                    concurrencies.reverse()
                for concurrency in concurrencies:
                    command = [
                        str(RUST_COMPARE), "http", "--base-url", f"http://127.0.0.1:{args.port}",
                        "--password", PASSWORD, "--encoded-track", token,
                        "--concurrency", str(concurrency), "--warmup-seconds", str(args.warmup_seconds),
                        "--measure-seconds", str(args.http_measure_seconds),
                    ]
                    result = run_measured(command, args.driver_cpus, observed_pid=process.pid)
                    result["implementation"] = implementation
                    result["trial"] = trial
                    result["idle_rss_kb"] = idle_rss
                    if result.get("cpu_seconds") and result["requests"]:
                        result["cpu_per_request_us"] = result["cpu_seconds"] * 1_000_000 / result["requests"]
                    driver_cpu = result.get("driver_cpu_seconds", 0)
                    result["driver_saturation"] = driver_cpu / (args.http_measure_seconds * len(parse_cpu_set(args.driver_cpus)))
                    result["valid"] = result["errors"] == 0 and result["driver_saturation"] < 0.8
                    results.append(result)
            finally:
                stop_server(process, log)
    return results, startup


def bootstrap_ci(ratios: list[float], samples: int = 10_000) -> tuple[float, float]:
    if not ratios:
        return math.nan, math.nan
    rng = random.Random(42)
    medians = [statistics.median(rng.choice(ratios) for _ in ratios) for _ in range(samples)]
    medians.sort()
    return medians[int(samples * 0.025)], medians[min(samples - 1, int(samples * 0.975))]


def verdict(ratios: list[float], higher_is_better: bool) -> tuple[str, float, float]:
    low, high = bootstrap_ci(ratios)
    if higher_is_better:
        status = "better" if low > 1.05 else "equivalent" if low >= 0.95 else "failed"
    else:
        status = "better" if high < 0.95 else "equivalent" if high <= 1.05 else "failed"
    return status, low, high


def paired_comparisons(results: list[dict]) -> list[dict]:
    metrics = {
        ("audio", "throughput"): [
            ("audio_seconds_per_wall_second", True),
            ("audio_seconds_per_cpu_second", True),
            ("peak_rss_kb", False),
            ("steady_rss_kb", False),
        ],
        ("http", None): [
            ("requests_per_second", True),
            ("p95_latency_us", False),
            ("p99_latency_us", False),
            ("cpu_per_request_us", False),
            ("idle_rss_kb", False),
            ("peak_rss_kb", False),
            ("steady_rss_kb", False),
        ],
    }
    groups: dict[tuple, dict[str, list[dict]]] = {}
    for result in results:
        if result.get("phase") == "capacity-discovery":
            continue
        key = (
            result.get("workload"), result.get("mode"), result.get("case"),
            result.get("input"), result.get("concurrency"), result.get("phase"),
        )
        groups.setdefault(key, {}).setdefault(result["implementation"], []).append(result)

    comparisons: list[dict] = []
    for key, implementations in sorted(groups.items(), key=lambda entry: str(entry[0])):
        if set(implementations) != {"rust", "java"}:
            continue
        metric_set = metrics.get((key[0], key[1])) or metrics.get((key[0], None))
        if not metric_set:
            continue
        rust = {item["trial"]: item for item in implementations["rust"]}
        java = {item["trial"]: item for item in implementations["java"]}
        for metric, higher in metric_set:
            ratios = []
            for trial in sorted(set(rust) & set(java)):
                rust_value = rust[trial].get(metric)
                java_value = java[trial].get(metric)
                if rust_value is not None and java_value not in (None, 0):
                    ratios.append(rust_value / java_value)
            if not ratios:
                comparisons.append({"workload": key, "metric": metric, "status": "invalid", "ratios": []})
                continue
            status, low, high = verdict(ratios, higher)
            comparisons.append({
                "workload": key,
                "metric": metric,
                "higher_is_better": higher,
                "ratios": ratios,
                "median_ratio": statistics.median(ratios),
                "ci95": [low, high],
                "status": status,
            })
    return comparisons


def bitrate_checks(results: list[dict]) -> list[dict]:
    grouped: dict[tuple, dict[str, list[float]]] = {}
    for result in results:
        if result.get("workload") != "audio" or result.get("mode") != "throughput":
            continue
        key = (result.get("case"), result.get("input"), result.get("concurrency"))
        grouped.setdefault(key, {}).setdefault(result["implementation"], []).append(result["output_bitrate"])
    checks = []
    for key, values in grouped.items():
        if set(values) != {"rust", "java"}:
            continue
        ratio = statistics.median(values["rust"]) / statistics.median(values["java"])
        checks.append({"workload": key, "rust_to_java": ratio, "status": "valid" if ratio >= 0.95 else "invalid"})
    return checks


def capacity_checks(capacity: dict) -> list[dict]:
    checks = []
    for case, values in capacity.items():
        rust = values["rust"]
        java = values["java"]
        passed = rust["verified"] and java["verified"] and rust["players"] >= java["players"]
        checks.append({"case": case, "rust": rust, "java": java, "status": "equivalent" if passed else "failed"})
    return checks


def command_output(command: list[str]) -> str:
    try:
        result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
        return (result.stdout or result.stderr).strip()
    except Exception as error:
        return f"unavailable: {error}"


def host_metadata(args: argparse.Namespace) -> dict:
    governors = {}
    for cpu in parse_cpu_set(args.server_cpus) + parse_cpu_set(args.driver_cpus):
        path = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor")
        governors[str(cpu)] = path.read_text().strip() if path.exists() else "unavailable"
    cpu_model = "unknown"
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            cpu_model = line.split(":", 1)[1].strip()
            break
    turbo_paths = [
        Path("/sys/devices/system/cpu/intel_pstate/no_turbo"),
        Path("/sys/devices/system/cpu/cpufreq/boost"),
    ]
    turbo = {str(path): path.read_text().strip() for path in turbo_paths if path.exists()}
    return {
        "platform": platform.platform(),
        "kernel": platform.release(),
        "cpu_model": cpu_model,
        "server_cpus": args.server_cpus,
        "driver_cpus": args.driver_cpus,
        "governors": governors,
        "turbo_controls": turbo,
        "rustc": command_output(["rustc", "--version"]),
        "java": command_output(["java", "-version"]),
        "git_commit": command_output(["git", "rev-parse", "HEAD"]),
        "git_dirty": bool(command_output(["git", "status", "--porcelain"])),
    }


def write_report(path: Path, document: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    markdown = [
        f"# Lavalink {UPSTREAM_VERSION} performance comparison",
        "",
        f"Overall verdict: **{document['summary']['status']}**",
        "",
        "| Workload | Metric | Rust/Java median | 95% CI | Verdict |",
        "|---|---:|---:|---:|---:|",
    ]
    for comparison in document["summary"]["comparisons"]:
        if not comparison.get("ratios"):
            ratio = ci = "n/a"
        else:
            ratio = f"{comparison['median_ratio']:.3f}"
            ci = f"{comparison['ci95'][0]:.3f}–{comparison['ci95'][1]:.3f}"
        markdown.append(f"| `{comparison['workload']}` | {comparison['metric']} | {ratio} | {ci} | {comparison['status']} |")
    markdown.extend(["", "Startup timing is informational and is not part of the verdict.", ""])
    path.with_suffix(".md").write_text("\n".join(markdown))


def execute(args: argparse.Namespace) -> None:
    ensure_linux(args)
    manifest = ensure_artifacts(args)
    audio_results: list[dict] = []
    http_results: list[dict] = []
    capacity: dict = {}
    startup: dict = {}
    if args.command in ("audio", "all"):
        audio_results, capacity = audio_suite(args, manifest)
    if args.command in ("http", "all"):
        http_results, startup = http_suite(args)
    results = audio_results + http_results
    comparisons = paired_comparisons(results)
    bitrate = bitrate_checks(audio_results)
    capacities = capacity_checks(capacity) if capacity else []
    valid_http = all(result.get("valid", True) for result in http_results)
    statuses = [item["status"] for item in comparisons] + [item["status"] for item in bitrate] + [item["status"] for item in capacities]
    overall = "passed" if statuses and all(status in ("better", "equivalent", "valid") for status in statuses) and valid_http else "failed"
    document = {
        "schema_version": 1,
        "run": {"timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "command": args.command},
        "host": host_metadata(args),
        "artifacts": {"upstream_version": UPSTREAM_VERSION, "upstream_sha256": UPSTREAM_SHA256, "fixtures": manifest},
        "workloads": results,
        "startup_seconds": startup,
        "summary": {"status": overall, "comparisons": comparisons, "bitrate_checks": bitrate, "capacity_checks": capacities},
    }
    write_report(Path(args.output), document)
    print(f"wrote {args.output} and {Path(args.output).with_suffix('.md')}")
    if overall != "passed":
        raise SystemExit(2)


def self_test() -> None:
    assert parse_cpu_set("0,2-4") == [0, 2, 3, 4]
    assert verdict([1.0, 1.0, 1.0], True)[0] == "equivalent"
    assert verdict([1.10, 1.11, 1.12], True)[0] == "better"
    assert verdict([1.10, 1.11, 1.12], False)[0] == "failed"
    equal = {"frames": 9_990, "misses": 10, "p99_service_us": 20_000}
    assert stable(equal)
    failed = {"frames": 9_989, "misses": 11, "p99_service_us": 20_000}
    assert not stable(failed)
    assert not stable({"frames": 0, "misses": 0, "p99_service_us": 0})
    assert invalid_measurement(1, True, {})
    assert invalid_measurement(0, False, {})
    assert invalid_measurement(0, True, None)
    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory) / "fixture"
        fixture.write_bytes(b"abc")
        assert sha256(fixture) == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    print("benchmark runner self-test passed")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    subparsers = root.add_subparsers(dest="command", required=True)
    subparsers.add_parser("prepare", help="download, verify and compile all benchmark artifacts")
    for name in ["audio", "http", "all"]:
        command = subparsers.add_parser(name)
        command.add_argument("--rust-bin", default=str(ROOT / "target" / "release" / ("lavalink-server.exe" if os.name == "nt" else "lavalink-server")))
        command.add_argument("--server-cpus", required=True)
        command.add_argument("--driver-cpus", required=True)
        command.add_argument("--output", default=str(WORK / "result.json"))
        command.add_argument("--port", type=int, default=2333)
        command.add_argument("--trials", type=int, default=3)
        command.add_argument("--warmup-seconds", type=int, default=60)
        command.add_argument("--audio-measure-seconds", type=int, default=60)
        command.add_argument("--http-measure-seconds", type=int, default=120)
        command.add_argument("--realtime-seconds", type=int, default=300)
        command.add_argument("--discovery-seconds", type=int, default=60)
        command.add_argument("--capacity-max-multiplier", type=int, default=8)
        command.add_argument("--allow-unstable-host", action="store_true")
    return root


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        self_test()
    else:
        arguments = parser().parse_args()
        if arguments.command == "prepare":
            prepare()
        else:
            execute(arguments)
