"""Render this project's SVG icon using Pillow; not needed when building Rust."""

from pathlib import Path
import xml.etree.ElementTree as ET

from PIL import Image, ImageColor, ImageDraw


directory = Path(__file__).resolve().parent
root = ET.parse(directory / "win-loop.svg").getroot()
scale = 4
side = int(root.attrib["width"]) * scale
image = Image.new("RGBA", (side, side))
stops = list(root.findall(".//{*}stop"))
colors = [ImageColor.getrgb(stop.attrib["stop-color"]) for stop in stops]


def render(node, inherited_fill=None):
    tag = node.tag.split("}")[-1]
    fill = node.get("fill", inherited_fill)
    if tag in ("svg", "g"):
        for child in node:
            render(child, fill)
        return
    if tag not in ("rect", "circle"):
        return
    mask = Image.new("L", image.size)
    drawing = ImageDraw.Draw(mask)
    if tag == "rect":
        x, y, width, height, radius = (
            float(node.get(key, 0)) * scale for key in ("x", "y", "width", "height", "rx")
        )
        drawing.rounded_rectangle((x, y, x + width, y + height), radius, fill=255)
    else:
        x, y, radius = (float(node.get(key)) * scale for key in ("cx", "cy", "r"))
        drawing.ellipse((x - radius, y - radius, x + radius, y + radius), fill=255)
    if fill.startswith("url("):
        layer = Image.new("RGBA", image.size)
        pixels = layer.load()
        for row in range(side):
            for column in range(side):
                fraction = max(0, min(1, ((column - x) / width + (row - y) / height) / 2))
                pixels[column, row] = tuple(
                    round(start + (end - start) * fraction)
                    for start, end in zip(*colors)
                ) + (255,)
    else:
        layer = Image.new("RGBA", image.size, fill)
    image.paste(layer, (0, 0), mask)


render(root)
image = image.resize((256, 256), Image.Resampling.LANCZOS if hasattr(Image, "Resampling") else Image.LANCZOS)
image.save(directory / "win-loop.png")
image.save(directory / "win-loop.ico", sizes=[(size, size) for size in (16, 20, 24, 32, 40, 48, 64, 128, 256)])
