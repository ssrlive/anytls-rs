#!/usr/bin/env python3
import argparse
import asyncio


BODY = b"hello-from-backend"


async def handle_client(reader, writer):
    try:
        await reader.readuntil(b"\r\n\r\n")
        headers = (
            b"HTTP/1.1 200 OK\r\n"
            + f"Content-Length: {len(BODY)}\r\n".encode()
            + b"Connection: close\r\n"
            + b"Content-Type: text/plain\r\n\r\n"
        )
        writer.write(headers)
        await writer.drain()
        await asyncio.sleep(0.02)
        writer.write(BODY)
        await writer.drain()
    except (ConnectionError, asyncio.IncompleteReadError, asyncio.LimitOverrunError):
        pass
    finally:
        writer.close()
        await writer.wait_closed()


async def serve(port):
    server = await asyncio.start_server(handle_client, "127.0.0.1", port)
    async with server:
        await server.serve_forever()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("port", type=int)
    args = parser.parse_args()
    asyncio.run(serve(args.port))


if __name__ == "__main__":
    main()