#!/usr/bin/env python3
"""把 src/tui/render.rs 里 dump_screens_for_docs 导出的 JSON 画成 PNG。

TUI 是固定网格的，所以按「每格固定像素宽高」逐格画字最省事：
双宽字符（中文）本来就占两格，直接按两格宽度绘制即可对齐。
"""
import json
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

CJK_FONT = "/System/Library/Fonts/Hiragino Sans GB.ttc"
MONO_FONT = "/System/Library/Fonts/SFNSMono.ttf"

CELL_W = 9          # 每格宽度（像素）
CELL_H = 19         # 行高
PAD = 14            # 外边框留白


def load_fonts(size: int):
    cjk = ImageFont.truetype(CJK_FONT, size)
    mono = ImageFont.truetype(MONO_FONT, size)
    return cjk, mono


def is_wide(ch: str) -> bool:
    code = ord(ch)
    return (
        0x1100 <= code <= 0x115F
        or 0x2E80 <= code <= 0xA4CF
        or 0xAC00 <= code <= 0xD7A3
        or 0xF900 <= code <= 0xFAFF
        or 0xFE30 <= code <= 0xFE6F
        or 0xFF00 <= code <= 0xFF60
        or 0xFFE0 <= code <= 0xFFE6
        or 0x20000 <= code <= 0x3FFFD
    )


def render(spec_path: Path, out_path: Path) -> None:
    spec = json.loads(spec_path.read_text())
    width, height = spec["width"], spec["height"]
    img_w = width * CELL_W + PAD * 2
    img_h = height * CELL_H + PAD * 2

    img = Image.new("RGB", (img_w, img_h), "#1c1c1c")
    draw = ImageDraw.Draw(img)
    cjk, mono = load_fonts(14)

    # 每格先铺背景色（相邻同色合并成一段，省得画几千个矩形）
    rows = {}
    for cell in spec["cells"]:
        rows.setdefault(cell["y"], []).append(cell)
    for y, cells in rows.items():
        cells.sort(key=lambda c: c["x"])
        run_start, run_color = None, None
        for cell in cells + [None]:
            color = cell["bg"] if cell else None
            if color != run_color:
                if run_color not in (None, "#1c1c1c") and run_start is not None:
                    x0 = PAD + run_start * CELL_W
                    x1 = PAD + (cell["x"] if cell else width) * CELL_W
                    draw.rectangle(
                        [x0, PAD + y * CELL_H, x1 - 1, PAD + (y + 1) * CELL_H - 1],
                        fill=run_color,
                    )
                run_color = color
                run_start = cell["x"] if cell else None

    # 再逐格画字
    for cell in spec["cells"]:
        ch = cell["ch"]
        if ch == " ":
            continue
        # 箭头等符号 SFNSMono 里没有（会画成 .notdef），交给中文字体
        arrows = "↑↓←→▲▼◀▶"
        font = cjk if (is_wide(ch) or ch in arrows) else mono
        # mono 字体缺少箭头之类的符号时退回中文字体
        if not is_wide(ch) and ch not in " " and mono.getbbox(ch) is None:
            font = cjk
        x = PAD + cell["x"] * CELL_W
        y = PAD + cell["y"] * CELL_H
        draw.text((x, y), ch, font=font, fill=cell["fg"])

    img.save(out_path)
    print(f"{spec_path.name} -> {out_path} ({img_w}x{img_h})")


def main() -> int:
    src_dir = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/shots")
    out_dir = Path(sys.argv[2] if len(sys.argv) > 2 else src_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    for name in ("form-empty", "form-filled", "form-expanded", "form-date-format",
             "form-date-format-small", "form-buttons", "form-small", "form-tiny",
             "browser", "picker", "prompt", "run-finished"):
        spec = src_dir / f"{name}.json"
        if spec.exists():
            render(spec, out_dir / f"{name}.png")
    return 0


if __name__ == "__main__":
    sys.exit(main())
