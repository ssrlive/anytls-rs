"""Utility helpers for smoke/integration scripts.
Provides: find_free_port, wait_for_port, start_proc, terminate_proc, ensure_cert
"""
from pathlib import Path
import socket
import time
import os
import subprocess
import sys
import shutil
import signal

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = Path(__file__).resolve().parent
CERT = SCRIPTS / "selfsigned.crt"
KEY = SCRIPTS / "selfsigned.key"


def find_free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def wait_for_port(host, port, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except Exception:
            time.sleep(0.1)
    return False


def start_proc(cmd, stdout_path=None, detach=True):
    f = None
    if stdout_path:
        f = open(stdout_path, "ab")

    kwargs = {}
    if os.name == 'nt':
        try:
            creation = subprocess.CREATE_NEW_PROCESS_GROUP
        except AttributeError:
            creation = 0x00000200
        DETACHED = 0x00000008
        if detach:
            kwargs['creationflags'] = creation | DETACHED
    else:
        if detach:
            kwargs['start_new_session'] = True

    proc = subprocess.Popen(cmd, stdout=f or subprocess.DEVNULL, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, **kwargs)
    return proc, f


def terminate_proc(proc, name=None, timeout=2.0):
    if not proc:
        return
    try:
        proc.terminate()
    except Exception:
        pass
    try:
        proc.wait(timeout=timeout)
        return
    except subprocess.TimeoutExpired:
        try:
            proc.kill()
        except Exception:
            pass
    if os.name == 'nt':
        try:
            if name and shutil.which('taskkill'):
                subprocess.run(["taskkill", "/IM", name, "/F"], check=False)
            elif shutil.which('taskkill'):
                subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], check=False)
        except Exception:
            pass
    else:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            pass


def cert_is_valid():
    if not CERT.exists() or not shutil.which('openssl'):
        return False
    try:
        output = subprocess.check_output(
            ["openssl", "x509", "-in", str(CERT), "-noout", "-text"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return False

    if 'X509v3 Basic Constraints' not in output:
        return False
    if 'CA:FALSE' not in output:
        return False
    if 'DNS:localhost' not in output and 'DNS: localhost' not in output:
        return False
    if 'IP Address:127.0.0.1' not in output and 'IP Address: 127.0.0.1' not in output:
        return False
    return True


def ensure_cert():
    if CERT.exists() and KEY.exists() and cert_is_valid():
        return True
    genpy = SCRIPTS / "gen_cert.py"
    if genpy.exists() and shutil.which(sys.executable):
        subprocess.run([sys.executable, str(genpy)])
        return CERT.exists() and KEY.exists() and cert_is_valid()
    if shutil.which('openssl'):
        cmd = [
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-sha256", "-days", "3650", "-subj", "/CN=localhost",
            "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-addext", "basicConstraints=CA:FALSE",
            "-addext", "extendedKeyUsage=serverAuth",
            "-keyout", str(KEY), "-out", str(CERT)
        ]
        subprocess.run(cmd)
        return CERT.exists() and KEY.exists() and cert_is_valid()
    return False


def kill_processes_by_name(name: str):
    """Kill processes by executable name. Works on Windows (taskkill) and Unix (pkill).

    This is best-effort and will ignore errors.
    """
    try:
        if os.name == 'nt':
            # /F force, /IM image name
            if shutil.which('taskkill'):
                subprocess.run(['taskkill', '/F', '/IM', name], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            else:
                # fallback: try tasklist to find PIDs then kill
                out = subprocess.check_output(['tasklist', '/FI', f'IMAGENAME eq {name}'], stderr=subprocess.DEVNULL, text=True)
                for line in out.splitlines():
                    parts = line.split()
                    if parts and parts[0].lower() == name.lower():
                        try:
                            pid = int(parts[1])
                            subprocess.run(['taskkill', '/PID', str(pid), '/F'], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                        except Exception:
                            pass
        else:
            # Unix-like: use pkill if available, otherwise pgrep+kill
            if shutil.which('pkill'):
                subprocess.run(['pkill', '-f', name], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            elif shutil.which('pgrep') and shutil.which('kill'):
                try:
                    out = subprocess.check_output(['pgrep', '-f', name], text=True)
                    for pid in out.splitlines():
                        try:
                            subprocess.run(['kill', '-9', pid], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                        except Exception:
                            pass
                except Exception:
                    pass
    except Exception:
        pass
