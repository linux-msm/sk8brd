#!/usr/bin/env python3
import argparse
import sys
import time
import uuid


def marker_exit_code(output, marker):
    for line in output.splitlines():
        if marker not in line:
            continue
        _, _, suffix = line.rpartition(f"{marker}:")
        try:
            return int(suffix)
        except ValueError:
            continue
    return None


def read_until(session, done, description, timeout_secs):
    deadline = time.monotonic() + timeout_secs
    output = ""

    while time.monotonic() < deadline:
        chunk = session.read_console()
        if chunk:
            print(chunk, end="", flush=True)
            output += chunk
            result = done(output)
            if result is not None:
                return output, result
        time.sleep(0.1)

    raise TimeoutError(f"timed out waiting for {description}")


def parse_args():
    parser = argparse.ArgumentParser(
        description="Boot an image through CDBA and run one console command.",
    )
    parser.add_argument(
        "--host",
        required=True,
        help="CDBA server hostname or IP address",
    )
    parser.add_argument("--board", required=True, help="CDBA board name")
    parser.add_argument("--image", required=True, help="boot image to upload")
    parser.add_argument("--port", type=int, default=22, help="CDBA SSH port")
    parser.add_argument("--user", default="cdba", help="CDBA SSH user")
    parser.add_argument(
        "--timeout",
        type=int,
        default=120,
        help="timeout in seconds for boot prompt and command completion",
    )
    parser.add_argument(
        "--prompt",
        default="root@qcom-armv8a:~#",
        help="shell prompt to wait for after boot",
    )
    parser.add_argument(
        "--command",
        default="uname -a; id",
        help="shell command to run after boot",
    )
    return parser.parse_args()


def main():
    args = parse_args()
    import sk8brd_cdba

    timeout_secs = args.timeout
    prompt = args.prompt
    command = args.command
    client = sk8brd_cdba.CdbaClient(
        args.host,
        port=args.port,
        user=args.user,
        timeout_secs=timeout_secs,
    )

    print(client.list_devices(), end="")
    session = client.boot_image_session(args.board, args.image)
    try:
        read_until(session, lambda output: True if prompt in output else None, prompt, timeout_secs)

        marker = f"__BOOT_LINUX_DONE_{uuid.uuid4().hex}__"
        session.write_console(f"\n{command}\nprintf '\\n{marker}:%s\\n' \"$?\"\n")
        _, exit_code = read_until(
            session,
            lambda output: marker_exit_code(output, marker),
            f"{marker}:<exit-code>",
            timeout_secs,
        )
    finally:
        session.close()

    print(f"\ncommand exit code: {exit_code}")
    return exit_code


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as err:
        print(err, file=sys.stderr)
        raise SystemExit(1)
