#!/usr/bin/env python3
"""Build the luckylode.cc/wiki static site from wiki/*.md.

Usage: python3 wiki/build.py   (writes wiki/dist/*.html)
Deploys to /var/www/html/wiki/ on the VPS (static files, no service).

Conventions: Home.md -> index.html, every other Page-Name.md ->
Page-Name.html, intra-wiki [text](Page-Name) links rewritten to match.
Same dark palette as the pool/explorer/faucet pages.
"""
import html
import os
import re
import sys

try:
    from markdown_it import MarkdownIt
except ImportError:
    sys.exit("need markdown-it-py (pip install markdown-it-py)")

SRC = os.path.join(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(SRC, "dist")

CSS = (
    ":root{--bg:#0d1117;--card:#161b22;--line:#30363d;--fg:#e6edf3;"
    "--dim:#8b949e;--green:#7ee787;--gold:#ffd166}"
    "*{box-sizing:border-box}"
    "body{background:var(--bg);color:var(--fg);"
    "font:14px ui-monospace,SFMono-Regular,Menlo,monospace;"
    "margin:0 auto;padding:24px;max-width:800px;line-height:1.65}"
    "nav{margin-bottom:18px;font-size:13px;line-height:2}"
    "nav a{color:var(--dim);text-decoration:none;margin-right:12px}"
    "nav a:hover{color:var(--gold)}"
    "h1{font-size:22px;color:var(--gold)}h2{font-size:17px;color:var(--gold);margin-top:26px}"
    "a{color:var(--green)}code{background:#0b0f14;border:1px solid var(--line);"
    "border-radius:6px;padding:1px 5px;font-size:13px}"
    "pre{background:#0b0f14;border:1px solid var(--line);border-radius:8px;padding:12px;overflow-x:auto}"
    "pre code{border:0;padding:0}"
    "blockquote{border-left:3px solid var(--gold);margin:14px 0;padding:4px 14px;color:var(--dim)}"
    "table{border-collapse:collapse;margin:12px 0}th,td{border:1px solid var(--line);padding:6px 10px;text-align:left}"
    ".card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px;margin:12px 0}"
    "footer{color:var(--dim);font-size:12px;margin-top:28px}"
)

NAV = (
    '<nav><a href="index.html">wiki</a>'
    '<a href="Getting-Started.html">start</a>'
    '<a href="Wallet.html">wallet</a>'
    '<a href="Pool-Mining.html">pool</a>'
    '<a href="Staking.html">staking</a>'
    '<a href="Luckynodes.html">luckynodes</a>'
    '<a href="Faucet.html">faucet</a>'
    '<a href="Game-Layer.html">game</a>'
    '<a href="Android-Miner.html">android</a>'
    '<a href="FAQ.html">faq</a>'
    '<a href="Testnet-Rules.html">rules</a>'
    '<a href="https://luckylode.cc/">luckylode.cc</a></nav>'
)

FOOT = (
    "<footer>Lucky Lode wiki · testnet — "
    "<a href=\"https://github.com/kdsmith18542/LuckyLode\">source</a> · "
    "<a href=\"https://github.com/kdsmith18542/LuckyLode/releases\">downloads</a></footer>"
)

PAGE = (
    "<!doctype html><html><head><meta charset=utf-8>"
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">"
    "<title>{title} — Lucky Lode wiki</title>"
    "<style>{css}</style></head><body>{nav}{body}{foot}</body></html>"
)


def out_name(src_name):
    stem = src_name[:-3]
    return "index.html" if stem == "Home" else stem + ".html"


def main():
    md = MarkdownIt()
    files = sorted(f for f in os.listdir(SRC) if f.endswith(".md"))
    if not files:
        sys.exit("no .md sources in wiki/")
    os.makedirs(DIST, exist_ok=True)

    def link_re(m):
        text, target = m.group(1), m.group(2)
        if "://" in target or target.startswith("#") or target.startswith("mailto:"):
            return m.group(0)
        if target.endswith(".md"):
            target = target[:-3]
        return f"[{text}]({target}.html)"

    for name in files:
        with open(os.path.join(SRC, name)) as f:
            src = f.read()
        # Intra-wiki links point at page stems; map them to built files.
        src = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", link_re, src)
        # Home.md is the index; its self-name never appears as a target,
        # but fix it if it does.
        src = src.replace("(Home.html)", "(index.html)")
        body = md.render(src)
        title = os.path.splitext(name)[0].replace("-", " ")
        with open(os.path.join(DIST, out_name(name)), "w") as f:
            f.write(PAGE.format(title=html.escape(title), css=CSS, nav=NAV, body=body, foot=FOOT))
        print("built", out_name(name))


if __name__ == "__main__":
    main()
