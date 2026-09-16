#!/usr/bin/env python3
"""Launch 100 concurrent curl processes through a SOCKS5H proxy.

This script uses your exact proxy command style and requests 100 distinct real URLs.
"""

import os
import shutil
import subprocess
import sys

PROXY = "socks5h://127.0.0.1:7080"
TIMEOUT = "30"

URLS = [
    "https://www.google.com/",
    "https://www.youtube.com/",
    "https://www.facebook.com/",
    "https://www.twitter.com/",
    "https://www.instagram.com/",
    "https://www.linkedin.com/",
    "https://www.wikipedia.org/",
    "https://www.yahoo.com/",
    "https://www.reddit.com/",
    "https://www.netflix.com/",
    "https://www.amazon.com/",
    "https://www.microsoft.com/",
    "https://www.apple.com/",
    "https://www.ebay.com/",
    "https://www.msn.com/",
    "https://www.pinterest.com/",
    "https://www.twitch.tv/",
    "https://www.spotify.com/",
    "https://www.wordpress.com/",
    "https://www.adobe.com/",
    "https://www.github.com/",
    "https://www.stackoverflow.com/",
    "https://www.quora.com/",
    "https://www.dropbox.com/",
    "https://www.cnn.com/",
    "https://www.bbc.com/",
    "https://www.nytimes.com/",
    "https://www.forbes.com/",
    "https://www.bloomberg.com/",
    "https://www.cnbc.com/",
    "https://www.wsj.com/",
    "https://t66y.com/",
    "https://www.theguardian.com/",
    "https://www.espn.com/",
    "https://www.foxnews.com/",
    "https://www.nbcnews.com/",
    "https://www.hulu.com/",
    "https://www.airbnb.com/",
    "https://www.booking.com/",
    "https://www.tripadvisor.com/",
    "https://www.expedia.com/",
    "https://www.hotels.com/",
    "https://www.trivago.com/",
    "https://www.uber.com/",
    "https://www.lyft.com/",
    "https://www.paypal.com/",
    "https://www.stripe.com/",
    "https://www.shopify.com/",
    "https://www.walmart.com/",
    "https://www.target.com/",
    "https://www.sohu.com/",
    "https://www.ikea.com/",
    "https://www.hm.com/",
    "https://www.zara.com/",
    "https://crates.io/",
    "https://www.nike.com/",
    "https://www.adidas.com/",
    "https://www.samsung.com/",
    "https://www.sony.com/",
    "https://www.dell.com/",
    "https://www.hp.com/",
    "https://www.lenovo.com/",
    "https://www.pornhub.com/",
    "https://www.toshiba.com/",
    "https://www.cisco.com/",
    "https://www.oracle.com/",
    "https://www.ibm.com/",
    "https://www.intel.com/",
    "https://www.baidu.com/",
    "https://www.nvidia.com/",
    "https://www.python.org/",
    "https://www.rust-lang.org/",
    "https://www.kubernetes.io/",
    "https://www.docker.com/",
    "https://www.gitlab.com/",
    "https://bitbucket.org/",
    "https://www.jira.com/",
    "https://www.zendesk.com/",
    "https://www.slack.com/",
    "https://www.zoom.us/",
    "https://www.trello.com/",
    "https://www.notion.so/",
    "https://www.medium.com/",
    "https://news.ycombinator.com/",
    "https://www.techcrunch.com/",
    "https://www.theverge.com/",
    "https://www.engadget.com/",
    "https://www.arstechnica.com/",
    "https://www.wired.com/",
    "https://www.pcmag.com/",
    "https://www.cnet.com/",
    "https://www.gizmodo.com/",
    "https://www.mashable.com/",
    "https://www.lifehacker.com/",
    "https://www.irs.gov/",
    "https://www.usa.gov/",
    "https://www.cdc.gov/",
    "https://www.nih.gov/",
    "https://www.who.int/",
    "https://www.gov.uk/",
    "https://www.mozilla.org/",
    "https://www.jetbrains.com/",
    "https://www.digitalocean.com/",
    "https://www.heroku.com/",
    "https://www.cloudflare.com/",
    "https://www.paytm.com/",
    "https://www.flipkart.com/",
]

URLS = URLS[:100]


def run_requests():
    processes = []
    for url in URLS:
        cmd = [
            "curl",
            "--proxy",
            PROXY,
            "--max-time",
            TIMEOUT,
            "-L",
            "-sS",
            "-o",
            os.devnull,
            "-w",
            "%{http_code}",
            url,
        ]
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        processes.append((url, proc))

    results = []
    for url, proc in processes:
        stdout, stderr = proc.communicate()
        status = proc.returncode
        output = stdout.decode(errors="replace").strip()
        if status == 0:
            results.append((url, True, output))
        else:
            err_text = stderr.decode(errors="replace").strip()
            results.append((url, False, f"exit={status} output={output} err={err_text}"))
    return results


def main():
    if not shutil.which("curl"):
        print("curl not found in PATH. Please install curl.")
        sys.exit(1)

    print("Starting 100 concurrent curl requests through SOCKS5H proxy...")
    print(f"proxy={PROXY}")

    results = run_requests()
    ok = sum(1 for _, success, _ in results if success)
    bad = len(results) - ok

    for url, success, message in results:
        prefix = "OK" if success else "FAIL"
        print(f"[{prefix}] {url} -> {message}")

    print(f"\nSummary: {ok} succeeded, {bad} failed, total {len(results)}")
    sys.exit(0 if bad == 0 else 1)


if __name__ == "__main__":
    import shutil

    main()
