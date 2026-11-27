"""Run the real three-client dev export proof on one Linux ARM host."""

from __future__ import annotations

import dataclasses
import importlib.util
import json
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace


@dataclasses.dataclass(frozen=True)
class Input:
    checkout: str
    scratch: str
    blaze: str
    dist_local: str
    serve: str
    blobs: str
    cores: int = 2
    timeout: int = 900


def load(argument: str) -> Input:
    if argument == "-":
        raw = json.load(sys.stdin)
    else:
        raw = json.loads(argument if argument.lstrip().startswith("{") else Path(argument).read_text())
    given = Input(**raw)
    for path in (given.checkout, given.scratch, given.blaze, given.dist_local, given.serve):
        if not isinstance(path, str) or not Path(path).is_absolute() or "\0" in path:
            raise ValueError(f"an absolute path is required: {path!r}")
    if type(given.cores) is not int or given.cores < 1 or type(given.timeout) is not int or not 0 < given.timeout <= 3600:
        raise ValueError("cores must be positive and timeout must be 1–3600 seconds")
    return given


def harness(checkout: str):
    script = Path(checkout) / "harness" / "distv2.py"
    sys.path.insert(0, str(script.parent))
    spec = importlib.util.spec_from_file_location("blaze_distv2_host_smoke", script)
    if spec is None or spec.loader is None:
        raise ValueError(f"cannot load the distributed harness from {script}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    names = ("artifact", "cache_address", "connected", "fixture", "node_identity",
             "prove", "run_export", "send", "shared_grants", "stop", "wait_ready", "worker_env", "write_json")
    return SimpleNamespace(
        script=script, evidence=module.evidence, Worker=module.HostWorker, Messages=module.HostMessages,
        hash_file=module.file_hash,
        **{name: getattr(module, f"host_{name}") for name in names},
    )


def local_proof(given: Input, hosts) -> None:
    evidence = hosts.evidence
    hosts.cache_address(given.blobs)
    root = Path(given.scratch)
    root.mkdir(parents=True, exist_ok=False)
    blobs = root / "blobs"
    coordinator_events = root / "coordinator.events.jsonl"
    processes = []
    with tempfile.TemporaryDirectory(prefix="blaze-3client-", dir="/tmp") as carrier:
        socket = Path(carrier) / "coordinator.sock"

        def worker(name: str, role: str):
            return hosts.Worker(
                checkout=given.checkout, scratch=str(root / name), blaze=given.blaze,
                python=sys.executable, runner=name, socket=str(socket), blobs=given.blobs,
                role=role, namespace=Path(carrier).name, cores=given.cores, timeout=given.timeout,
            )

        def start(argv: list[str], log: Path, *, env=None, stdin=subprocess.DEVNULL, messages=False):
            with log.open("xb") as output:
                process = subprocess.Popen(
                    argv, env=env, stdin=stdin, stdout=subprocess.PIPE if messages else output,
                    stderr=output, start_new_session=True,
                )
            processes.append(process)
            return process

        try:
            control = worker("solo", "primary")
            solo_root = Path(control.scratch)
            solo_root.mkdir()
            project = solo_root / "project"
            source, expected = hosts.fixture(project)
            env = hosts.worker_env(solo_root, given.cores)
            identity = hosts.node_identity(control, env, source)
            service_binaries = {"blaze-dist-local": Path(given.dist_local), "blaze-serve": Path(given.serve)}
            service_hashes = {name: hosts.hash_file(path) for name, path in service_binaries.items()}
            begin = time.monotonic_ns()
            solo_process = hosts.run_export(control, solo_root, project, env)
            processes.append(solo_process)
            evidence.require(solo_process.wait(timeout=given.timeout) == 0,
                             f"solo export failed; see {solo_root / 'export.log'}")
            solo_ns = time.monotonic_ns() - begin
            solo = hosts.artifact(solo_root, expected, given.timeout)

            evidence.require(not hosts.connected(given.blobs), "cache port is already in use")
            cache = start([given.serve, "--dir", str(blobs), "--listen", given.blobs], root / "serve.log")
            hosts.wait_ready(cache, lambda: hosts.connected(given.blobs), time.monotonic() + 30, "native cache")
            coordinator = start(
                [given.dist_local, "coordinate", str(socket), identity["triple"], "64"], root / "coordinator.log",
                env={**env, "BLAZE_DIST_EVENTS": str(coordinator_events)}, stdin=subprocess.PIPE,
            )
            hosts.wait_ready(coordinator, socket.is_socket, time.monotonic() + 30, "coordinator UDS")

            sessions = []
            for name, role in (("primary", "primary"), ("helper-1", "helper"), ("helper-2", "helper")):
                request = worker(name, role)
                process = start(
                    [request.python, str(hosts.script), json.dumps(dataclasses.asdict(request))],
                    root / f"{name}.session.log", stdin=subprocess.PIPE, messages=True,
                )
                sessions.append((process, hosts.Messages(process.stdout, name)))

            deadline = time.monotonic() + given.timeout
            identities = [channel.receive("prepared", deadline)["identity"] for _, channel in sessions]
            same = identity.keys() - {"runner"}
            evidence.require(all(node.keys() == identity.keys() and all(node[field] == identity[field] for field in same)
                                 for node in identities),
                             "local workers must share the real boot, source, compiler and fixture")
            hosts.write_json(root / "local-workers.json", identities)

            deadline = time.monotonic() + given.timeout
            hosts.send(sessions[0][0].stdin, {"phase": "start"})
            held = [sessions[0][1].receive("held", deadline)]
            for process, _ in sessions[1:]:
                hosts.send(process.stdin, {"phase": "start"})
            held.extend(channel.receive("held", deadline) for _, channel in sessions[1:])
            hosts.shared_grants(held)
            hosts.write_json(root / "held.json", held)
            for process, _ in sessions:
                hosts.send(process.stdin, {"phase": "release"})
            completed = [channel.receive("complete", deadline) for _, channel in sessions]
            for (process, _), node, result in zip(sessions, identities, completed, strict=True):
                evidence.require(result["identity"] == node and process.wait(timeout=max(0.001, deadline - time.monotonic())) == 0,
                                 f"{node['runner']}: identity changed or session failed")
            events = evidence.jsonl(coordinator_events)
            evidence.validate_events(events)
            proof = hosts.prove(blobs, held, completed, events, solo)
            evidence.require(service_hashes == {name: hosts.hash_file(path) for name, path in service_binaries.items()},
                             "service binaries changed during the run")
            hosts.write_json(root / "single-host-three-client-proof.json", {
                "scope": "three local worker processes with isolated homes on one host",
                "workers": identities, "solo_artifact": solo, "solo_export_ns": solo_ns,
                "service_binaries_sha256": service_hashes,
                "gated_worker_export_ns": [row["elapsed_ns"] for row in completed],
                "proof": proof,
            })
            print("distv2: single-host three-client protocol and primary placement checks passed", flush=True)
        finally:
            for process in reversed(processes):
                hosts.stop(process)


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("pass one JSON object, file or '-'")
        given = load(sys.argv[1])
        hosts = harness(given.checkout)
        signal.signal(signal.SIGTERM, lambda signum, _: sys.exit(f"interrupted by signal {signum}"))
        try:
            local_proof(given, hosts)
        except hosts.evidence.Failed as why:
            sys.exit(f"single-host three-client smoke: {why}")
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as why:
        sys.exit(f"single-host three-client smoke: {why}")
