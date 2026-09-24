#!/usr/bin/env python3
"""
Generate a 10-year self-signed certificate and private key (RSA 2048).
Writes `selfsigned.crt` and `selfsigned.key` into the scripts/ directory.
Requires OpenSSL available on PATH.
"""
import subprocess
import shutil
from pathlib import Path
import sys

SCRIPTS = Path(__file__).resolve().parent
CRT = SCRIPTS / "selfsigned.crt"
KEY = SCRIPTS / "selfsigned.key"

def cert_is_valid() -> bool:
    if not shutil.which('openssl'):
        return False
    try:
        output = subprocess.check_output(
            ['openssl', 'x509', '-in', str(CRT), '-noout', '-text'],
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


def main():
    if CRT.exists() and KEY.exists():
        if cert_is_valid():
            print("Certificate + key already exist and are valid:", CRT, KEY)
            return 0
        print("Existing certificate is invalid or not a proper server cert; regenerating.")

    if not shutil.which('openssl'):
        print('openssl not found; please install OpenSSL or create certs manually', file=sys.stderr)
        return 2
    cmd = [
        'openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
        '-sha256', '-days', '3650', '-subj', '/CN=localhost',
        '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
        '-addext', 'basicConstraints=CA:FALSE',
        '-addext', 'extendedKeyUsage=serverAuth',
        '-keyout', str(KEY), '-out', str(CRT)
    ]
    print('Running:', ' '.join(cmd))
    res = subprocess.run(cmd)
    if res.returncode != 0:
        print('openssl failed', file=sys.stderr)
        return res.returncode
    print('Wrote:', CRT, KEY)
    return 0

if __name__ == '__main__':
    sys.exit(main())
