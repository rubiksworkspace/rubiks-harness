#!/usr/bin/env python3
"""The agent phase's only way out: an HTTPS (CONNECT) proxy that reaches the
hosts named on the command line and refuses everything else.

The agent container sits on an internal Docker network whose only other
member is this proxy, so the model API is reachable and nothing else is (the
SWE-bench Pro locked protocol: no network for the agent, except its model).
Every request is logged, allowed or refused, one JSON line per connection, as
evidence of what the agent tried to reach.

Usage: egress_proxy.py [--port 8888] host [host ...]
"""
import argparse
import asyncio
import json
import sys
import time


def log(event, **fields):
    print(json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "event": event, **fields}), flush=True)


async def pipe(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    except (ConnectionError, asyncio.IncompleteReadError):
        pass
    finally:
        writer.close()


async def handle(reader, writer, allowed):
    request = (await reader.readline()).decode("latin-1").split()
    while (line := await reader.readline()) not in (b"\r\n", b"\n", b""):
        pass
    method = request[0].upper() if request else ""
    target = request[1] if len(request) > 1 else ""
    host, _, port = target.rpartition(":")
    if method != "CONNECT" or host.lower() not in allowed or port != "443":
        log("refused", method=method, target=target)
        writer.write(b"HTTP/1.1 403 Forbidden\r\n\r\n")
        await writer.drain()
        writer.close()
        return
    try:
        upstream_reader, upstream_writer = await asyncio.open_connection(host, 443)
    except OSError as error:
        log("upstream_failed", target=target, error=str(error))
        writer.write(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
        await writer.drain()
        writer.close()
        return
    log("allowed", target=target)
    writer.write(b"HTTP/1.1 200 Connection established\r\n\r\n")
    await writer.drain()
    await asyncio.gather(pipe(reader, upstream_writer), pipe(upstream_reader, writer))


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8888)
    parser.add_argument("hosts", nargs="+")
    args = parser.parse_args()
    allowed = {host.lower() for host in args.hosts}
    server = await asyncio.start_server(lambda r, w: handle(r, w, allowed), "0.0.0.0", args.port)
    log("listening", port=args.port, allowed=sorted(allowed))
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)
