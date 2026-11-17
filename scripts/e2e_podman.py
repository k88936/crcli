#!/usr/bin/env python3
"""Run the crcli end-to-end test against an ephemeral Cloudreve container.

Adapted from cloudreve-sdk-rs/scripts/e2e_podman.py: start a disposable
Cloudreve container, register the first user as administrator, then exercise
`crcli login` and `crcli upload` and verify the result through the API.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


REPOSITORY = Path(__file__).resolve().parent.parent
IMAGE = os.environ.get(
    "CLOUDREVE_E2E_IMAGE", "docker.io/cloudreve/cloudreve:4.19.1"
)
CONTAINER = f"crcli-e2e-{os.getpid()}"
EMAIL = "e2e-admin@example.test"
PASSWORD = "e2e-password-change-me"
BINARY = REPOSITORY / "target" / "debug" / "crcli"
REMOTE_ROOT = "cloudreve://my/"


def run(*command: str, capture_output: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=REPOSITORY,
        check=True,
        text=True,
        capture_output=capture_output,
    )


def api_request(
    url: str,
    body: dict[str, str] | None = None,
    token: str | None = None,
) -> object:
    data = json.dumps(body).encode() if body is not None else None
    headers: dict[str, str] = {}
    if data is not None:
        headers["Content-Type"] = "application/json"
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, data=data, headers=headers)
    with urllib.request.urlopen(request, timeout=10) as response:
        payload = json.load(response)
    if payload.get("code") != 0:
        raise RuntimeError(f"Cloudreve API request failed: {payload!r}")
    return payload.get("data")


def wait_until_ready(server: str) -> None:
    deadline = time.monotonic() + 90
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            version = api_request(f"{server}/api/v4/site/ping")
            print(f"Cloudreve {version} is ready", flush=True)
            return
        except (OSError, RuntimeError, ValueError, urllib.error.URLError) as error:
            last_error = error
            time.sleep(1)
    raise TimeoutError(f"Cloudreve did not become ready: {last_error}")


def image_available(image: str) -> bool:
    return (
        subprocess.run(
            ("podman", "image", "exists", image),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def remove_container() -> None:
    if shutil.which("podman") is None:
        return
    subprocess.run(
        ("podman", "rm", "--force", CONTAINER),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def stop_on_signal(_signum: int, _frame: object) -> None:
    raise KeyboardInterrupt


def list_folder(server: str, uri: str) -> dict[str, int]:
    """Return {name: size} for the entries of a folder."""
    login = api_request(
        f"{server}/api/v4/session/token",
        {"email": EMAIL, "password": PASSWORD},
    )
    token = login["token"]["access_token"]
    quoted = urllib.parse.quote(uri, safe="")
    listing = api_request(
        f"{server}/api/v4/file?uri={quoted}&page_size=100", token=token
    )
    return {file["name"]: file["size"] for file in listing["files"]}


def crcli(environment: dict[str, str], *arguments: str) -> None:
    subprocess.run(
        (str(BINARY), *arguments),
        cwd=REPOSITORY,
        env=environment,
        check=True,
    )


def main() -> None:
    for command in ("podman", "cargo"):
        if shutil.which(command) is None:
            raise RuntimeError(f"required command not found: {command}")

    if image_available(IMAGE):
        print(f"Reusing the local Cloudreve image {IMAGE}", flush=True)
    else:
        print(f"Pulling Cloudreve image {IMAGE}", flush=True)
        run("podman", "pull", IMAGE)

    print(f"Starting ephemeral Cloudreve container from {IMAGE}", flush=True)
    run(
        "podman",
        "run",
        "--detach",
        "--rm",
        "--pull",
        "never",
        "--image-volume",
        "ignore",
        "--name",
        CONTAINER,
        "--publish",
        "127.0.0.1::5212",
        IMAGE,
        capture_output=True,
    )

    inspection = json.loads(
        run("podman", "inspect", CONTAINER, capture_output=True).stdout
    )[0]
    mounts = inspection.get("Mounts", [])
    if mounts:
        raise RuntimeError(f"ephemeral container unexpectedly has mounts: {mounts!r}")
    print("Verified that the container has no mounts", flush=True)

    port = run(
        "podman", "port", CONTAINER, "5212/tcp", capture_output=True
    ).stdout.strip()
    server = f"http://{port}"
    print(f"Waiting for Cloudreve at {server}", flush=True)
    wait_until_ready(server)

    print("Registering the first user as administrator", flush=True)
    api_request(
        f"{server}/api/v4/user",
        {"email": EMAIL, "password": PASSWORD, "language": "en-US"},
    )

    print("Building crcli", flush=True)
    run("cargo", "build", "--quiet")

    # Run the CLI with a throwaway HOME so ~/cloudreve-cli.toml is never touched.
    with tempfile.TemporaryDirectory(prefix="crcli-e2e-home-") as home:
        environment = os.environ.copy()
        environment["HOME"] = home

        print("Running crcli login", flush=True)
        crcli(
            environment,
            "login",
            "--server",
            server,
            "--email",
            EMAIL,
            "--password",
            PASSWORD,
        )

        config_path = Path(home) / "cloudreve-cli.toml"
        if not config_path.is_file():
            raise RuntimeError(f"login did not write {config_path}")
        print(f"Verified that login wrote {config_path}", flush=True)

        payload_root = Path(home) / "payload"
        payload_root.mkdir()
        single_name = f"crcli-e2e-single-{os.getpid()}.txt"
        single_file = payload_root / single_name
        single_content = "crcli end-to-end upload\n" * 4096
        single_file.write_text(single_content)

        nested_dir = payload_root / "tree"
        nested_dir.mkdir()
        nested_name = f"crcli-e2e-nested-{os.getpid()}.bin"
        nested_file = nested_dir / "sub" / nested_name
        nested_file.parent.mkdir()
        nested_content = bytes(range(256)) * 4096
        nested_file.write_bytes(nested_content)

        print("Running crcli upload for a single file", flush=True)
        crcli(environment, "upload", str(single_file), REMOTE_ROOT)

        print("Running crcli upload recursively for a directory", flush=True)
        crcli(
            environment,
            "upload",
            str(nested_dir),
            f"{REMOTE_ROOT}crcli-e2e-tree-{os.getpid()}",
            "--recursive",
        )

        print("Verifying the uploads through the API", flush=True)
        root_files = list_folder(server, REMOTE_ROOT)
        tree_name = f"crcli-e2e-tree-{os.getpid()}"

        single_size = len(single_content.encode())
        if root_files.get(single_name) != single_size:
            raise RuntimeError(
                f"{single_name} has size {root_files.get(single_name)}, expected {single_size}"
            )
        if tree_name not in root_files:
            raise RuntimeError(f"{tree_name} was not created: {root_files!r}")

        nested_root = list_folder(server, f"{REMOTE_ROOT}{tree_name}/")
        if nested_root.get("sub") is None:
            raise RuntimeError(f"{tree_name}/sub was not created: {nested_root!r}")

        nested_files = list_folder(server, f"{REMOTE_ROOT}{tree_name}/sub/")
        if nested_files.get(nested_name) != len(nested_content):
            raise RuntimeError(
                f"{nested_name} was not uploaded below {tree_name}/sub: {nested_files!r}"
            )

        print(f"Verified {single_name} ({single_size} bytes) and {nested_name}", flush=True)

    print("E2E test passed", flush=True)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, stop_on_signal)
    failed = False
    try:
        main()
    except BaseException:
        failed = True
        raise
    finally:
        if failed:
            subprocess.run(
                ("podman", "logs", CONTAINER),
                stdout=sys.stderr,
                stderr=subprocess.DEVNULL,
                check=False,
                text=True,
            )
        remove_container()
