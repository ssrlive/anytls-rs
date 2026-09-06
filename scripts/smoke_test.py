#!/usr/bin/env python3
import os
import json
import sys
import subprocess
import signal
import time
import socket
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
CERT = SCRIPTS / "selfsigned.crt"
#!/usr/bin/env python3
import os
import sys
import subprocess
import time
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
CERT = SCRIPTS / "selfsigned.crt"
KEY = SCRIPTS / "selfsigned.key"
SERVER_LOG = SCRIPTS / "server.log"
S_CLIENT_OUT = SCRIPTS / "s_client.out"
INTEGRATION_OUT = SCRIPTS / "integration_client.out"
INTEGRATION_ERR = SCRIPTS / "integration_client.err"
LOG_LEVEL = os.environ.get('SMOKE_LOG', 'info')
INTERACTIVE = os.environ.get('SMOKE_INTERACTIVE', '0') == '1'
KEEP_PROCS = os.environ.get('SMOKE_KEEP_PROCS', '0') == '1'

# allow overriding via CLI flags for automation environments
if '--log' in sys.argv:
    try:
        idx = sys.argv.index('--log')
        LOG_LEVEL = sys.argv[idx + 1]
    except Exception:
        pass
if '--interactive' in sys.argv:
    INTERACTIVE = True
if '--keep-procs' in sys.argv or '--no-cleanup' in sys.argv:
    KEEP_PROCS = True

# Simple help output for CI or local use
if '--help' in sys.argv or '-h' in sys.argv:
        print("""
Usage: python scripts/smoke_test.py [OPTIONS]

Options:
    -h, --help            Show this help message and exit
    --interactive         Run in interactive mode (writes scripts/interactive.json)
    --keep-procs          Leave server/client/http processes running after the test
    --no-cleanup          Alias for --keep-procs
    --log <LEVEL>         Set log level (e.g. info, debug). Also honored via SMOKE_LOG

Environment variables:
    SMOKE_INTERACTIVE=1   Same as --interactive
    SMOKE_KEEP_PROCS=1    Same as --keep-procs
    SMOKE_LOG=<LEVEL>     Same as --log

Examples:
    python scripts/smoke_test.py --keep-procs
    SMOKE_KEEP_PROCS=1 python scripts/smoke_test.py
""")
        sys.exit(0)

# ensure repo root is on sys.path so `scripts` package is importable when running this file
sys.path.insert(0, str(ROOT))

# import helpers from scripts.utils
from scripts.utils import (
    find_free_port,
    wait_for_port,
    start_proc,
    terminate_proc,
    ensure_cert,
)


def run(cmd, **kwargs):
    return subprocess.run(cmd, shell=False, check=False, **kwargs)


def main():
    os.chdir(ROOT)
    ok = ensure_cert()
    if not ok:
        print("Certificate not available; aborting")
        sys.exit(1)

    port = find_free_port()
    print(f"Using TLS port {port}")

    print("Building anytls-server and anytls-client")
    run([shutil.which('cargo') or 'cargo', 'build', '--bin', 'anytls-server'])
    run([shutil.which('cargo') or 'cargo', 'build', '--bin', 'anytls-client'])

    bin_server = ROOT / 'target' / 'debug' / 'anytls-server'
    if (bin_server.with_suffix('.exe')).exists():
        bin_server = bin_server.with_suffix('.exe')
    bin_client = ROOT / 'target' / 'debug' / 'anytls-client'
    if (bin_client.with_suffix('.exe')).exists():
        bin_client = bin_client.with_suffix('.exe')

    print(f"Starting anytls-server: {bin_server}")
    srv_proc, srv_f = start_proc([str(bin_server), '--password', 'testpass', '--cert', str(CERT), '--key', str(KEY), '--listen', f'127.0.0.1:{port}', '--log', LOG_LEVEL], stdout_path=str(SERVER_LOG))
    time.sleep(1)

    print("Running openssl s_client handshake check")
    if shutil.which('openssl'):
        # Use Popen with stdin=DEVNULL and a short timeout to avoid blocking
        with open(S_CLIENT_OUT, 'wb') as f:
            proc = subprocess.Popen(['openssl', 's_client', '-connect', f'127.0.0.1:{port}', '-servername', 'localhost', '-quiet'], stdout=f, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                terminate_proc(proc, name='openssl.exe' if os.name=='nt' else 'openssl')
        try:
            print(Path(S_CLIENT_OUT).read_text()[:10240])
        except Exception:
            pass
    else:
        print("openssl not found; skipping s_client check")

    print("== Full integration test: anytls-client <-> anytls-server with local HTTP backend ==")
    http_port = find_free_port()
    socks_port = find_free_port()
    print(f"HTTP {http_port}, SOCKS {socks_port}")

    print(f"Starting local HTTP server on 127.0.0.1:{http_port}")
    http_proc, _ = start_proc([sys.executable, str(SCRIPTS / 'integration_server.py'), str(http_port)])
    if not wait_for_port('127.0.0.1', http_port, timeout=3.0):
        print('HTTP backend did not start in time')
        terminate_proc(http_proc, name='python.exe' if os.name == 'nt' else 'python')
        terminate_proc(srv_proc, name=bin_server.name)
        sys.exit(1)

    if not wait_for_port('127.0.0.1', port, timeout=3.0):
        print('Server did not start in time; dumping server log')
        if SERVER_LOG.exists():
            print(SERVER_LOG.read_text())
        terminate_proc(srv_proc, name=bin_server.name)
        terminate_proc(http_proc, name='python.exe' if os.name=='nt' else 'python')
        sys.exit(1)

    # reuse the already-started server instance for integration

    print(f"Starting anytls-client pointing to server, exposing SOCKS5 on 127.0.0.1:{socks_port}")
    client_log = SCRIPTS / 'client.log'
    cl_proc, cl_f = start_proc([
        str(bin_client),
        '--server', f'127.0.0.1:{port}',
        '--password', 'testpass',
        '--sni', 'localhost',
        '--root-cert', str(CERT),
        '--listen', f'socks5://127.0.0.1:{socks_port}',
        '--log', LOG_LEVEL,
    ], stdout_path=str(client_log))

    if INTERACTIVE:
        print('\nInteractive mode: processes started.')
        print(f'Server TLS port: {port}, HTTP backend: {http_port}, SOCKS port: {socks_port}')
        # write ports to a file so external runner can pick them up
        try:
            (SCRIPTS / 'interactive.json').write_text(json.dumps({'tls_port': port, 'http_port': http_port, 'socks_port': socks_port}))
        except Exception:
            pass
        print('Inspect logs in scripts/server.log and scripts/client.log. Press Enter to terminate.')
        try:
            input()
        except Exception:
            pass

        print('Cleaning up processes (interactive)')
        for p, name in [(cl_proc, bin_client.name), (http_proc, 'python.exe' if os.name=='nt' else 'python'), (srv_proc, bin_server.name)]:
            try:
                terminate_proc(p, name=name)
            except Exception:
                pass

        for fh in (srv_f, cl_f) if 'srv_f' in locals() else []:
            try:
                fh and fh.close()
            except Exception:
                pass

        sys.exit(0)

    # wait for the SOCKS listener to be ready
    if not wait_for_port('127.0.0.1', socks_port, timeout=3.0):
        print('Client SOCKS listener did not start in time; dumping server log')
        if SERVER_LOG.exists():
            print(SERVER_LOG.read_text())
            if not KEEP_PROCS:
                terminate_proc(cl_proc, name=bin_client.name)
                terminate_proc(srv_proc, name=bin_server.name)
        terminate_proc(http_proc, name='python.exe' if os.name=='nt' else 'python')
        sys.exit(1)

    print("Testing HTTP fetch via SOCKS5 proxy")
    used = False
    if shutil.which('curl'):
        try:
            with open(INTEGRATION_OUT, 'wb') as stdout, open(INTEGRATION_ERR, 'wb') as stderr:
                curl = run(
                    ['curl', '--socks5', f'127.0.0.1:{socks_port}', '--max-time', '10', '-sS', f'http://127.0.0.1:{http_port}/'],
                    stdout=stdout,
                    stderr=stderr,
                    timeout=12,
                )
            print(f'curl exit code: {curl.returncode}')
            if curl.returncode != 0:
                print(Path(INTEGRATION_ERR).read_text(errors='replace'))
            used = True
        except subprocess.TimeoutExpired:
            print('curl timed out')
            used = True
    if not used:
        print('curl not found or not used; please run the integration fetch manually via socks proxy')
    else:
        out = Path(INTEGRATION_OUT).read_text()
        print(out)

    success = 'hello-from-backend' in (Path(INTEGRATION_OUT).read_text() if Path(INTEGRATION_OUT).exists() else '')

    if success:
        print('Integration test passed: fetched content via proxy')
        status = 0
    else:
        print('Integration test failed: backend content not fetched')
        if SERVER_LOG.exists():
            print('Server log (tail):')
            print('\n'.join(SERVER_LOG.read_text().splitlines()[-200:]))
        status = 1
        if client_log.exists():
            print('\nClient log (tail):')
            print('\n'.join(client_log.read_text().splitlines()[-200:]))

    print('Cleaning up processes')
    if not KEEP_PROCS:
        for p, name in [(cl_proc, bin_client.name), (http_proc, 'python.exe' if os.name=='nt' else 'python'), (srv_proc, bin_server.name)]:
            try:
                terminate_proc(p, name=name)
            except Exception:
                pass
    else:
        print('Processes left running for manual inspection:')
        try:
            print(f'  server pid={srv_proc.pid} (log={SERVER_LOG})')
        except Exception:
            pass
        try:
            print(f'  client pid={cl_proc.pid} (log={client_log})')
        except Exception:
            pass
        try:
            print(f'  http pid={http_proc.pid}')
        except Exception:
            pass

    if not KEEP_PROCS:
        for fh in (srv_f, cl_f) if 'srv_f' in locals() else []:
            try:
                fh and fh.close()
            except Exception:
                pass
    # best-effort: ensure no stray anytls processes remain from prior detached runs
    if not KEEP_PROCS:
        try:
            from scripts.utils import kill_processes_by_name
            # Windows executables
            kill_processes_by_name('anytls-client.exe')
            kill_processes_by_name('anytls-server.exe')
            # fallback names (unix / no .exe)
            kill_processes_by_name('anytls-client')
            kill_processes_by_name('anytls-server')
        except Exception:
            pass
    sys.exit(status)


if __name__ == '__main__':
    main()

