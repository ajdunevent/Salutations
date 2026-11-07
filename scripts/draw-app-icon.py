# Writes the Salutations app icon: three stacked envelopes in
# AdwAvatar colours, GNOME app icon style.
import os

def shade(hexc, f):
    c = [int(hexc[i:i+2], 16) for i in (1, 3, 5)]
    if f < 1:
        c = [int(v * f) for v in c]
    else:
        c = [int(v + (255 - v) * (f - 1)) for v in c]
    return '#%02x%02x%02x' % tuple(c)

BLUE = ('#cfe1f5', '#83b6ec', '#337fdc')
GREEN = ('#cef8d8', '#8de6b1', '#29ae74')

W, H, R, T = 104, 64, 12, 7   # envelope size, corner radius, thickness

def envelope(i, x, y, col, lines=True):
    fg, top, bottom = col
    base = shade(bottom, 0.72)
    out = f'''
  <linearGradient id="face{i}" x1="0" y1="{y}" x2="0" y2="{y+H}" gradientUnits="userSpaceOnUse">
    <stop offset="0" stop-color="{top}"/><stop offset="1" stop-color="{bottom}"/></linearGradient>
  <linearGradient id="side{i}" x1="{x}" y1="0" x2="{x+W}" y2="0" gradientUnits="userSpaceOnUse">
    <stop offset="0" stop-color="{shade(base, 1.35)}"/><stop offset="0.07" stop-color="{base}"/>
    <stop offset="0.93" stop-color="{base}"/><stop offset="1" stop-color="{shade(base, 1.35)}"/></linearGradient>
  <linearGradient id="hl{i}" x1="0" y1="{y}" x2="0" y2="{y+H}" gradientUnits="userSpaceOnUse">
    <stop offset="0" stop-color="#fff" stop-opacity="0.55"/><stop offset="0.25" stop-color="#fff" stop-opacity="0"/></linearGradient>
  <rect x="{x}" y="{y+T}" width="{W}" height="{H}" rx="{R}" fill="url(#side{i})"/>
  <rect x="{x}" y="{y}" width="{W}" height="{H}" rx="{R}" fill="url(#face{i})"/>
  <rect x="{x+0.75}" y="{y+0.75}" width="{W-1.5}" height="{H-1.5}" rx="{R-0.75}" fill="none" stroke="url(#hl{i})" stroke-width="1.5"/>'''
    if lines:
        # Only the front envelope shows its flap; the ones behind show
        # their faces alone.
        out += f'''
  <path d="M {x+16} {y+H-14} L {x+W*0.4} {y+H*0.56} M {x+W-16} {y+H-14} L {x+W*0.6} {y+H*0.56}"
        fill="none" stroke="{fg}" stroke-opacity="0.35" stroke-width="3.5" stroke-linecap="round"/>
  <path d="M {x+14} {y+14} L {x+W/2} {y+H*0.6} L {x+W-14} {y+14}"
        fill="none" stroke="#ffffff" stroke-opacity="0.85" stroke-width="5" stroke-linecap="round" stroke-linejoin="round"/>'''
    return out

def icon(name, places, colors):
    body = ''
    # Back to front.
    for i, ((x, y), col) in reversed(list(enumerate(zip(places, colors)))):
        body += envelope(i, x, y, col, lines=(i == 0))
    svg = f'<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">{body}\n</svg>\n'
    open(name, 'w').write(svg)

VIOLET = ('#d5d2f5', '#9e91e8', '#7a59ca')
X = (128 - W) / 2
OFF = 20
# Each envelope sits SHIFT further left than the one in front of it; the
# stack as a whole stays centred.
SHIFT = 6
top = (128 - (H + 2 * OFF + T)) / 2
icon(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'salutations', 'data', 'icons', 'scalable', 'apps', 'net.dunevent.Salutations.svg'), [(X + SHIFT, top), (X, top + OFF), (X - SHIFT, top + 2 * OFF)], [GREEN, BLUE, VIOLET])
