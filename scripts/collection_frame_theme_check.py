#!/usr/bin/env python3
"""Check that the collection frame follows the app's light or dark colours.

    collection_frame_theme_check.py <screenshot folder>

The check opens no port and starts no Harold. It writes static HTML files to a
temp folder. Each file holds the app.css text and a `.collection-frame` iframe
with a sample page in srcdoc. Command-line headless Chrome takes one screenshot
per file and per colour mode. Python reads the pixels of the screenshots.
"""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import zlib

ROOT = Path(__file__).resolve().parent.parent
CSS = (ROOT / 'harold/static/web_app/app.css').read_text()
CHROME = os.environ.get(
    'CHROME', '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome')
WIDTH, HEIGHT = 390, 844

SURFACE = {'light': (0xf8, 0xf9, 0xfa), 'dark': (0x03, 0x05, 0x07)}

# The page that follows the explainer rule: tokens, and a dark set.
TOKEN_PAGE = '''<!doctype html><meta charset="utf-8">
<style>
:root { color-scheme: light dark; --page: #ffffff; --ink: #111111; }
@media (prefers-color-scheme: dark) { :root { --page: #123456; --ink: #eeeeee; } }
body { background: var(--page); color: var(--ink); font: 20px sans-serif; margin: 0; padding: 12px; }
</style><body><p>Token sample page</p>'''
# The page that sets no colour and no color-scheme.
PLAIN_PAGE = '''<!doctype html><meta charset="utf-8">
<body style="font:20px sans-serif;margin:0;padding:12px"><p>Plain sample page</p>'''

# A page that declares color-scheme and sets no background: the canvas is clear,
# so the frame's own background shows through.
EMPTY_PAGE = '<!doctype html><meta charset="utf-8"><style>:root { color-scheme: light dark; }</style>'

PAGES = {'empty': EMPTY_PAGE, 'tokens': TOKEN_PAGE, 'plain': PLAIN_PAGE}


def attr(text):
    return text.replace('&', '&amp;').replace('"', '&quot;')


def write_html(folder, name, page):
    html = f'''<!doctype html><meta charset="utf-8">
<meta name="viewport" content="width=device-width">
<style>{CSS}</style>
<body style="margin:0;display:flex;flex-direction:column;height:100vh">
<iframe class="collection-frame" sandbox srcdoc="{attr(page)}"></iframe>'''
    path = folder / f'{name}.html'
    path.write_text(html)
    return path


def shoot(html, png, dark):
    command = [
        CHROME, '--headless=new', '--disable-gpu', '--hide-scrollbars',
        f'--screenshot={png}', f'--window-size={WIDTH},{HEIGHT}',
        f'--blink-settings=preferredColorScheme={0 if dark else 1}',
        f'file://{html}',
    ]
    subprocess.run(command, check=True, capture_output=True, timeout=60)


def read_png(path):
    data = path.read_bytes()
    assert data[:8] == b'\x89PNG\r\n\x1a\n', f'{path} is not a PNG'
    pos, idat, width, height, depth, kind = 8, b'', 0, 0, 0, 0
    while pos < len(data):
        size = int.from_bytes(data[pos:pos + 4], 'big')
        tag = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + size]
        if tag == b'IHDR':
            width = int.from_bytes(body[0:4], 'big')
            height = int.from_bytes(body[4:8], 'big')
            depth, kind = body[8], body[9]
        elif tag == b'IDAT':
            idat += body
        pos += 12 + size
    assert depth == 8 and kind in (2, 6), 'only 8-bit RGB or RGBA is read'
    step = 3 if kind == 2 else 4
    raw = zlib.decompress(idat)
    rows, prev, pos = [], bytearray(width * step), 0
    for _ in range(height):
        filt = raw[pos]
        line = bytearray(raw[pos + 1:pos + 1 + width * step])
        pos += 1 + width * step
        for i in range(len(line)):
            left = line[i - step] if i >= step else 0
            up = prev[i]
            corner = prev[i - step] if i >= step else 0
            if filt == 1:
                line[i] = (line[i] + left) & 255
            elif filt == 2:
                line[i] = (line[i] + up) & 255
            elif filt == 3:
                line[i] = (line[i] + ((left + up) >> 1)) & 255
            elif filt == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                pred = left if pa <= pb and pa <= pc else up if pb <= pc else corner
                line[i] = (line[i] + pred) & 255
        rows.append(line)
        prev = line

    def pixel(x, y):
        return tuple(rows[y][x * step:x * step + 3])
    return width, height, pixel


def near(a, b, slack=3):
    return all(abs(x - y) <= slack for x, y in zip(a, b))


def main():
    out = Path(sys.argv[1]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        folder = Path(tmp)
        for name, page in PAGES.items():
            html = write_html(folder, name, page)
            for mode in ('dark', 'light'):
                png = out / f'collection-frame-{name}-{mode}-{WIDTH}.png'
                shoot(html, png, mode == 'dark')
                width, height, pixel = read_png(png)
                inside = pixel(width // 2, height - 40)
                print(f'{name} {mode}: pixel={inside}')
                if name == 'empty':
                    if inside == (255, 255, 255):
                        failures.append(f'{name} {mode}: frame background is #fff')
                    if not near(inside, SURFACE[mode]):
                        failures.append(f'{name} {mode}: frame {inside} is not the surface {SURFACE[mode]}')
                elif name == 'tokens':
                    want = (0x12, 0x34, 0x56) if mode == 'dark' else (255, 255, 255)
                    if not near(inside, want):
                        failures.append(f'{name} {mode}: page {inside} is not {want}')
                else:
                    if sum(inside) / 3 < 200:
                        failures.append(f'{name} {mode}: page {inside} is not light')
    if failures:
        print('FAIL')
        for line in failures:
            print(' ', line)
        return 1
    print('PASS')
    return 0


if __name__ == '__main__':
    sys.exit(main())
