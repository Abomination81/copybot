#!/usr/bin/env python3
"""Render the checked-in Markdown guides. No network requests or bot connection."""
from pathlib import Path
from html import escape
import re
import sys
from markdown_it import MarkdownIt

ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / 'docs'
GUIDES = ['INSTALL', 'CONFIGURATION', 'OPERATIONS', 'RELEASE-SCOPE']
renderer = MarkdownIt('commonmark').enable('table')

def render(name):
    source = (DOCS / (name + '.md')).read_text()
    title = source.splitlines()[0].removeprefix('# ')
    body = renderer.render(source)
    body = re.sub(r'href="([A-Z-]+)\.md', lambda m: 'href="' + m[1].lower() + '.html', body)
    links = ''.join('<a href="' + g.lower() + '.html"' + (' aria-current="page"' if g == name else '') + '>' + g.replace('-', ' ').title() + '</a>' for g in GUIDES)
    return f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="theme-color" content="#020704"><title>{escape(title)} · Abomination81 Copybot</title><link rel="icon" href="assets/copybot-mark.svg"><link rel="stylesheet" href="styles.css"><link rel="stylesheet" href="guide.css"></head>
<body><a class="skip" href="#guide">Skip to guide</a><header class="site-header wrap"><a class="brand" href="index.html"><img src="assets/copybot-mark.svg" width="42" height="42" alt=""><span>Abomination81 Copybot<small>Independent execution. Built to be yours.</small></span></a><nav aria-label="Main navigation"><a href="index.html">Project home</a><a href="https://x.com/Abomination81" target="_blank" rel="noopener noreferrer">X ↗</a><a class="nav-github" href="https://github.com/Abomination81/copybot">GitHub ↗</a></nav></header>
<main class="wrap guide-layout"><aside><p class="eyebrow">Operator guides</p><nav aria-label="Guide navigation">{links}</nav><p>Private source preview.<br>No live connection.</p></aside><article id="guide">{body}</article></main>
<footer class="wrap"><a href="index.html">Abomination81 Copybot</a><span>Independent software. Not affiliated with Polymarket.</span><a href="https://x.com/Abomination81">X @Abomination81 ↗</a></footer></body></html>
'''

def main():
    check = '--check' in sys.argv
    stale = []
    for name in GUIDES:
        target = DOCS / (name.lower() + '.html')
        output = render(name)
        if check:
            if not target.exists() or target.read_text() != output: stale.append(target.name)
        else: target.write_text(output)
    if stale: print('Regenerate documentation: ' + ', '.join(stale))
    else: print('Four documentation guides verified.' if check else 'Four documentation guides rendered.')
    return bool(stale)

if __name__ == '__main__': sys.exit(main())
