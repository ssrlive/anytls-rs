#!/usr/bin/env python3
"""
Run a local reproduction for UDP-over-TCP (UOT) using anytls-server and anytls-client.
"""
import os
import sys
import time
import socket
import struct
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / 'scripts'
ARTIFACTS = ROOT / 'target' / 'uot-local-py'
ARTIFACTS.mkdir(parents=True, exist_ok=True)

# Local test configuration
PASSWORD = 'password'
LISTEN_HOST = '127.0.0.1'

DEBUG_DIR = ROOT / 'target' / 'debug'
SERVER_BINARY = DEBUG_DIR / 'anytls-server'
CLIENT_BINARY = DEBUG_DIR / 'anytls-client'
if (SERVER_BINARY.with_suffix('.exe')).exists():
    SERVER_BINARY = SERVER_BINARY.with_suffix('.exe')
if (CLIENT_BINARY.with_suffix('.exe')).exists():
    CLIENT_BINARY = CLIENT_BINARY.with_suffix('.exe')

SERVER_STDOUT = ARTIFACTS / 'server.stdout.log'
CLIENT_STDOUT = ARTIFACTS / 'client.stdout.log'

# import helpers
sys.path.insert(0, str(ROOT))
from scripts.utils import ensure_cert, find_free_port, start_proc, terminate_proc

def udp_echo_server(sock: socket.socket, stop: threading.Event, ready: threading.Event):
    ready.set()
    try:
        while not stop.is_set():
            try:
                data, addr = sock.recvfrom(65535)
                sock.sendto(data, addr)
            except socket.timeout:
                continue
    finally:
        sock.close()


def read_exact(sock: socket.socket, size: int) -> bytes:
    buffer = bytearray()
    while len(buffer) < size:
        chunk = sock.recv(size - len(buffer))
        if not chunk:
            raise RuntimeError('unexpected EOF')
        buffer.extend(chunk)
    return bytes(buffer)


def wait_for_process_port(proc, host: str, port: int, log_path: Path, timeout: float = 10.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        return_code = proc.poll()
        if return_code is not None:
            log = log_path.read_text(errors='replace') if log_path.exists() else '(no log)'
            raise RuntimeError(f'{proc.args[0]} exited with status {return_code}:\n{log}')
        try:
            with socket.create_connection((host, port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.1)
    raise TimeoutError(f'{host}:{port} did not become ready')


def main():
    if not SERVER_BINARY.exists() or not os.access(str(SERVER_BINARY), os.X_OK):
        print(f"Server binary not found at {SERVER_BINARY}. Build it first: cargo build --bin anytls-server", file=sys.stderr)
        return 2
    if not CLIENT_BINARY.exists() or not os.access(str(CLIENT_BINARY), os.X_OK):
        print(f"Client binary not found at {CLIENT_BINARY}. Build it first: cargo build --bin anytls-client", file=sys.stderr)
        return 2

    if not ensure_cert():
        print('Certificate not available; build or generate it first: python scripts/gen_cert.py', file=sys.stderr)
        return 2

    server_port = find_free_port()
    client_port = find_free_port()
    server_listen = f'{LISTEN_HOST}:{server_port}'
    client_listen = f'{LISTEN_HOST}:{client_port}'
    udp_echo = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp_echo.bind((LISTEN_HOST, 0))
    udp_echo.settimeout(0.5)
    udp_echo_port = udp_echo.getsockname()[1]
    udp_stop = threading.Event()
    udp_ready = threading.Event()
    udp_thread = threading.Thread(target=udp_echo_server, args=(udp_echo, udp_stop, udp_ready), daemon=True)
    srv_proc = cl_proc = srv_f = cl_f = None

    try:
        print(f'Starting local UDP echo server on {LISTEN_HOST}:{udp_echo_port}')
        udp_thread.start()
        if not udp_ready.wait(timeout=2.0):
            raise RuntimeError('UDP echo server thread did not start')

        print(f'Starting anytls-server on {server_listen} from {SERVER_BINARY}')
        srv_cmd = [
            str(SERVER_BINARY),
            '-l', server_listen,
            '-p', PASSWORD,
            '--cert', str(SCRIPTS / 'selfsigned.crt'),
            '--key', str(SCRIPTS / 'selfsigned.key'),
            '--sni', 'localhost',
        ]
        srv_proc, srv_f = start_proc(srv_cmd, stdout_path=str(SERVER_STDOUT))
        wait_for_process_port(srv_proc, LISTEN_HOST, server_port, SERVER_STDOUT)

        print(f'Starting anytls-client on {client_listen} from {CLIENT_BINARY}')
        cl_cmd = [
            str(CLIENT_BINARY),
            '-l', client_listen,
            '-s', server_listen,
            '-p', PASSWORD,
            '--sni', 'localhost',
            '--root-cert', str(SCRIPTS / 'selfsigned.crt'),
        ]
        cl_proc, cl_f = start_proc(cl_cmd, stdout_path=str(CLIENT_STDOUT))
        wait_for_process_port(cl_proc, LISTEN_HOST, client_port, CLIENT_STDOUT)

        print('Opening SOCKS5 control connection')
        # connect to client's SOCKS5 control port
        with socket.create_connection((LISTEN_HOST, client_port), timeout=5.0) as control:
            control.settimeout(5.0)
            control.sendall(b"\x05\x01\x00")
            auth_reply = read_exact(control, 2)
            if auth_reply != b"\x05\x00":
                raise RuntimeError(f"SOCKS5 auth negotiation failed: {auth_reply!r}")

            # UDP ASSOCIATE: VER=5, CMD=3, RSV=0, ATYP=1 (IPv4) + 4 bytes addr + 2 bytes port (0.0.0.0:0 to let server pick)
            control.sendall(b"\x05\x03\x00\x01\x00\x00\x00\x00\x00\x00")
            reply_head = read_exact(control, 4)
            if reply_head[:2] != b"\x05\x00":
                raise RuntimeError(f"UDP ASSOCIATE failed: {reply_head!r}")

            # read address according to ATYP
            atyp = reply_head[3]
            if atyp == 1:
                addr = socket.inet_ntoa(read_exact(control, 4))
            elif atyp == 3:
                size = read_exact(control, 1)[0]
                addr = read_exact(control, size).decode('ascii')
            elif atyp == 4:
                addr = socket.inet_ntop(socket.AF_INET6, read_exact(control, 16))
            else:
                raise RuntimeError(f'Unsupported ATYP {atyp}')
            port_bytes = read_exact(control, 2)
            relay_port = int.from_bytes(port_bytes, 'big')
            relay_host = addr
            if relay_host in ('0.0.0.0', '::'):
                relay_host = LISTEN_HOST

            print(f'SOCKS5 UDP relay at {relay_host}:{relay_port}')

            # send a UDP packet via the relay to the local UDP echo server
            payload = b'anytls-uot-ok'
            # SOCKS5 UDP request header: RSV(2)=0, FRAG=0, ATYP=1, DST.ADDR (4), DST.PORT (2)
            packet = b"\x00\x00\x00\x01" + socket.inet_aton(LISTEN_HOST) + struct.pack('!H', udp_echo_port) + payload
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp_sock:
                udp_sock.settimeout(5.0)
                udp_sock.sendto(packet, (relay_host, relay_port))
                response, relay_source = udp_sock.recvfrom(65535)

            if relay_source != (relay_host, relay_port):
                raise RuntimeError(f'Unexpected UDP relay source {relay_source!r}')
            if len(response) < 10 or response[:3] != b"\x00\x00\x00":
                raise RuntimeError(f'Unexpected SOCKS5 UDP header: {response[:3]!r}')
            if response[3] != 1:
                raise RuntimeError(f'Unexpected UDP address type {response[3]}')
            source_ip = socket.inet_ntoa(response[4:8])
            source_port = int.from_bytes(response[8:10], 'big')
            response_payload = response[10:]

            if response_payload != payload:
                raise RuntimeError(f'Unexpected UDP payload: {response_payload!r}')
            if source_ip != LISTEN_HOST or source_port != udp_echo_port:
                raise RuntimeError(f'Unexpected UDP source {source_ip}:{source_port}')

        print('UDP ASSOCIATE end-to-end validation passed')
        return 0
    except Exception as e:
        print('Test failed:', e, file=sys.stderr)
        return 5
    finally:
        print('Cleaning up')
        udp_stop.set()
        if udp_thread.ident is not None:
            udp_thread.join(timeout=1.0)
        for proc, log_file in ((cl_proc, cl_f), (srv_proc, srv_f)):
            if proc is not None:
                terminate_proc(proc)
            if log_file is not None:
                log_file.close()


if __name__ == '__main__':
    sys.exit(main())
