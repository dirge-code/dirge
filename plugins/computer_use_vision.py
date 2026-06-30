#!/usr/bin/env python3
"""Computer-use vision analysis — local backend (default).

Edge detection + tesseract OCR. Runs entirely offline, zero API cost.
Returns structured JSON: text regions grouped into lines/blocks, buttons
with coordinates, window metadata, likely foreground application.

Pluggable vision architecture
------------------------------
The Janet computer_use plugin dispatches all vision through a single
function: (analyze-image path prompt). That function can route to any
backend by changing one body — no other code in the plugin changes.

  Backend 1 — local Python (this script, CURRENT DEFAULT)
      python3 computer_use_vision.py --path <file>
      Zero cost. No API key. Works offline.
      Trade-off: OCR-only, no semantic reasoning.

  Backend 2 — DeepSeek Vision (when /chat/completions adds image_url)
      curl + base64 → api.deepseek.com/v1/chat/completions
      Multimodal understanding. Native V4 architecture.
      Trade-off: not yet exposed via API (image_url rejected).

  Backend 3 — cloud multimodal APIs (Anthropic, OpenRouter/OpenAI)
      curl + base64 → claude / gpt-4o / openrouter
      Strongest semantic analysis. Button labeling, coordinate
      estimation, natural-language queries.
      Trade-off: API cost, network dependency.

To switch backends, replace the analyze-image body in computer_use.janet
with the matching curl/base64 pipeline.

Dependencies: Pillow, numpy, pytesseract, tesseract-ocr
Input: PNG screenshot path
Output: JSON on stdout
"""
import argparse, json, sys
from collections import defaultdict
import numpy as np
from PIL import Image, ImageFilter, ImageOps

# Tesseract may need path hints on some systems; keep it simple.
import pytesseract


def _boxes_overlap(a, b, margin=3):
    """True if boxes a and b overlap (with margin)."""
    ax1, ay1, ax2, ay2 = a["x"] - margin, a["y"] - margin, a["x"] + a["w"] + margin, a["y"] + a["h"] + margin
    bx1, by1, bx2, by2 = b["x"] - margin, b["y"] - margin, b["x"] + b["w"] + margin, b["y"] + b["h"] + margin
    ox = max(0, min(ax2, bx2) - max(ax1, bx1))
    oy = max(0, min(ay2, by2) - max(ay1, by1))
    return ox > 0 and oy > 0


def _merge_overlapping(regions):
    """Dedup: keep higher-confidence when two boxes overlap."""
    regions = sorted(regions, key=lambda r: (r["conf"], len(r["text"])), reverse=True)
    kept = []
    for r in regions:
        if any(_boxes_overlap(r, k) for k in kept):
            continue
        kept.append(r)
    return kept


def _preprocess_variants(image):
    """Yield (name, preprocessed_image) for multiple OCR strategies."""
    gray = image.convert("L")

    # Variant 1: light contrast + moderate threshold
    v1 = ImageOps.autocontrast(gray, cutoff=2)
    v1 = v1.filter(ImageFilter.SHARPEN)
    v1_bin = v1.point(lambda p: 255 if p > 120 else 0)
    yield ("light", v1_bin)

    # Variant 2: stronger contrast + higher threshold (good for UI labels)
    v2 = ImageOps.autocontrast(gray, cutoff=1)
    v2 = v2.filter(ImageFilter.SHARPEN)
    v2_bin = v2.point(lambda p: 255 if p > 150 else 0)
    yield ("medium", v2_bin)

    # Variant 3: inverted — dark text on light bg often missed
    v3 = ImageOps.invert(gray)
    v3 = ImageOps.autocontrast(v3, cutoff=1)
    v3_bin = v3.point(lambda p: 255 if p > 140 else 0)
    yield ("inverted", v3_bin)

    # Variant 4: plain grayscale with autocontrast (no binarize)
    v4 = ImageOps.autocontrast(gray, cutoff=1)
    yield ("grayscale", v4)

    # Variant 5: raw grayscale — tesseract's native preference
    yield ("raw", gray)


def ocr_text_multi(image):
    """Run tesseract on multiple preprocessed variants and merge results.

    Returns list of region dicts with block_num, par_num, line_num for grouping.
    """
    all_regions = []
    for variant_name, prep in _preprocess_variants(image):
        try:
            data = pytesseract.image_to_data(
                prep, output_type=pytesseract.Output.DICT,
                config="--psm 6"  # Assume uniform block of text
            )
        except Exception:
            continue
        n = len(data["text"])
        for i in range(n):
            txt = data["text"][i].strip()
            if not txt or len(txt) < 2:
                continue
            x, y, w, h = data["left"][i], data["top"][i], data["width"][i], data["height"][i]
            if not (w > 3 and h > 5):  # skip tiny artifacts
                continue
            conf = int(data["conf"][i]) if data["conf"][i] != "-1" else -1
            all_regions.append({
                "text": txt,
                "x": x, "y": y, "w": w, "h": h,
                "conf": conf,
                "line_num": data["line_num"][i],
                "par_num": data["par_num"][i],
                "block_num": data["block_num"][i],
            })
    return _merge_overlapping(all_regions)


def group_by_line(regions):
    """Group text regions into lines by (block_num, par_num, line_num)."""
    lines = defaultdict(list)
    for r in regions:
        key = (r.get("block_num", 0), r.get("par_num", 0), r.get("line_num", 0))
        lines[key].append(r)

    result = []
    for key in sorted(lines.keys()):
        items = sorted(lines[key], key=lambda r: r["x"])
        if not items:
            continue
        # Bounding box of entire line
        xs = [it["x"] for it in items]
        ys = [it["y"] for it in items]
        x2s = [it["x"] + it["w"] for it in items]
        y2s = [it["y"] + it["h"] for it in items]
        line_text = " ".join(it["text"] for it in items)
        avg_conf = int(sum(it["conf"] for it in items if it["conf"] > 0) / max(1, sum(1 for it in items if it["conf"] > 0)))
        result.append({
            "text": line_text,
            "x": min(xs), "y": min(ys),
            "w": max(x2s) - min(xs), "h": max(y2s) - min(ys),
            "conf": avg_conf,
            "word_count": len(items),
            "words": [it["text"] for it in items],
        })
    return result


def find_buttons(image):
    """Detect button-like UI elements by finding rectangular edges.

    Uses Canny-like edge detection and finds closed rectangular contours.
    More reliable than color saturation alone — catches flat-design buttons.
    """
    w, h = image.size

    # Strategy 1: edge-based button detection using PIL's built-in filters
    gray_img = image.convert("L")
    edges_img = gray_img.filter(ImageFilter.FIND_EDGES)
    edge_arr = np.array(edges_img, dtype=np.uint8)

    # Strong edges (bright in FIND_EDGES output) = likely UI boundaries
    edge_mask = (edge_arr > 40).astype(np.uint8)

    # Find connected components in edge mask
    # Simple flood-fill approach: scan for edge-dense regions
    h_edges, w_edges = edge_mask.shape
    visited = np.zeros_like(edge_mask, dtype=bool)
    buttons = []

    for sy in range(0, h_edges, 4):
        for sx in range(0, w_edges, 4):
            if edge_mask[sy, sx] == 0 or visited[sy, sx]:
                continue
            # BFS to find extent of connected edge region
            min_x, max_x = sx, sx
            min_y, max_y = sy, sy
            stack = [(sx, sy)]
            while stack:
                cx, cy = stack.pop()
                if not (0 <= cx < w_edges and 0 <= cy < h_edges):
                    continue
                if visited[cy, cx] or edge_mask[cy, cx] == 0:
                    continue
                visited[cy, cx] = True
                min_x, max_x = min(min_x, cx), max(max_x, cx)
                min_y, max_y = min(min_y, cy), max(max_y, cy)
                for dx, dy in [(-1,0),(1,0),(0,-1),(0,1)]:
                    stack.append((cx+dx, cy+dy))

            bw, bh = max_x - min_x + 1, max_y - min_y + 1
            if not (24 < bw < w * 0.7 and 16 < bh < h * 0.4):
                continue
            ratio = bw / max(bh, 1)
            if not (0.5 < ratio < 20):
                continue

            # Check edge density: real buttons have edges around border, not filling interior
            box = edge_mask[min_y:max_y+1, min_x:max_x+1]
            if bw > 6 and bh > 6:
                interior = box[2:-2, 2:-2].mean()
            else:
                interior = 0
            perimeter = box.mean()
            if perimeter > 0.015 and interior < 0.3:
                buttons.append({
                    "x": int(min_x), "y": int(min_y),
                    "w": int(bw), "h": int(bh),
                    "cx": int((min_x + max_x) // 2), "cy": int((min_y + max_y) // 2),
                })

    # Strategy 2: color-saturation flood-fill (complementary)

    # Also: detect large uniform-color rectangles (traditional approach)
    arr = np.array(image.convert("RGB"), dtype=np.float32)
    r, g, b = arr[:,:,0], arr[:,:,1], arr[:,:,2]
    gray_arr = 0.299 * r + 0.587 * g + 0.114 * b
    sat = np.sqrt((r - gray_arr)**2 + (g - gray_arr)**2 + (b - gray_arr)**2)

    # Downscale and flood-fill
    scale = 4
    colored = (sat > 35).astype(np.uint8) * 255
    small = np.array(Image.fromarray(colored).resize(
        (w // scale, h // scale), Image.Resampling.NEAREST
    ))

    visited = np.zeros_like(small, dtype=bool)
    for sy in range(0, small.shape[0], 3):
        for sx in range(0, small.shape[1], 3):
            if small[sy, sx] == 0 or visited[sy, sx]:
                continue
            min_x, max_x = sx, sx
            min_y, max_y = sy, sy
            stack = [(sx, sy)]
            while stack:
                cx, cy = stack.pop()
                if not (0 <= cx < small.shape[1] and 0 <= cy < small.shape[0]):
                    continue
                if visited[cy, cx] or small[cy, cx] == 0:
                    continue
                visited[cy, cx] = True
                min_x, max_x = min(min_x, cx), max(max_x, cx)
                min_y, max_y = min(min_y, cy), max(max_y, cy)
                for dx, dy in [(-1,0),(1,0),(0,-1),(0,1)]:
                    stack.append((cx+dx, cy+dy))

            bw, bh = (max_x - min_x + 1) * scale, (max_y - min_y + 1) * scale
            if 30 < bw < w * 0.7 and 15 < bh < h * 0.4:
                bx, by = min_x * scale, min_y * scale
                buttons.append({
                    "x": int(bx), "y": int(by),
                    "w": int(bw), "h": int(bh),
                    "cx": int(bx + bw // 2), "cy": int(by + bh // 2),
                })

    # Dedup by overlap
    buttons.sort(key=lambda b: b["w"] * b["h"], reverse=True)
    merged = []
    for btn in buttons:
        if any(_boxes_overlap(btn, m, margin=5) for m in merged):
            continue
        merged.append(btn)
    return merged[:20]


def find_foreground_window(image):
    """Heuristic: the top bar area with distinct background color."""
    w, h = image.size
    arr = np.array(image.convert("RGB"))

    top_strip = arr[2:28, w//4:3*w//4, :]
    mean_color = top_strip.mean(axis=(0, 1))

    mid_strip = arr[h//2:h//2+26, w//4:3*w//4, :]
    mid_mean = mid_strip.mean(axis=(0, 1))
    diff = float(np.sqrt(np.sum((mean_color - mid_mean) ** 2)))

    return {
        "has_title_bar": diff > 30,
        "title_bar_color": [int(c) for c in mean_color],
        "title_bar_rgb": "#{:02x}{:02x}{:02x}".format(*mean_color.astype(int)),
    }


def edge_density(image):
    """Estimate how much 'content' is on screen via edge density."""
    gray = image.convert("L")
    edges = gray.filter(ImageFilter.FIND_EDGES)
    arr = np.array(edges)
    return round(float((arr > 30).mean()), 4)


def analyze(path):
    img = Image.open(path).convert("RGB")
    w, h = img.size

    # Multi-pass OCR with automatic line grouping
    raw_regions = ocr_text_multi(img)
    lines = group_by_line(raw_regions)

    buttons = find_buttons(img)
    window_info = find_foreground_window(img)
    density = edge_density(img)

    all_text = " ".join(r["text"] for r in raw_regions[:100])

    # Likely foreground app
    likely_app = "unknown"
    app_clues = {
        "firefox": ["firefox", "mozilla", "new tab", "Firefox"],
        "chromium": ["chromium", "chrome", "Chrome"],
        "terminal": ["terminal", "bash", "zsh", "~$"],
        "cosmic-files": ["Files", "Home", "Documents"],
        "cosmic-edit": ["Untitled", "COSMIC Edit"],
        "vscode": ["Visual Studio", "Code"],
    }
    lower_all = all_text.lower()
    for app, clues in app_clues.items():
        if any(c.lower() in lower_all for c in clues):
            likely_app = app
            break

    result = {
        "width": w,
        "height": h,
        "edge_density": density,
        "likely_app": likely_app,
        "window": window_info,
        "text_regions": raw_regions[:50],
        "lines": lines,
        "line_count": len(lines),
        "text_summary": all_text[:800],
        "buttons": buttons[:15],
        "button_count": len(buttons),
    }
    return result


# ── find-app mode utilities ─────────────────────────────────────────
_APP_CLUES = {
    "firefox": ["firefox", "mozilla", "new tab", "Firefox"],
    "chromium": ["chromium", "chrome", "Chrome"],
    "browser": ["firefox", "mozilla", "new tab", "Firefox", "chromium", "chrome", "Chrome"],
    "terminal": ["terminal", "bash", "zsh", "~$"],
    "files": ["Files", "Home", "Documents", "Pictures"],
    "editor": ["Untitled", "Edit", "cosmic edit"],
}


def find_app(result, target):
    """Check if named app appears to be foreground. Returns 'yes' or 'no'."""
    clues = _APP_CLUES.get(target.lower(), [target])
    haystack = result.get("text_summary", "").lower()
    return "yes" if any(c.lower() in haystack for c in clues) else "no"


def find_main_button(result):
    """Return the button most likely to be the page's primary action.

    Strategy: among buttons near the vertical center of the page,
    pick the largest one. Falls back to the largest button overall.
    Returns (cx, cy) or (None, None).
    """
    buttons = result.get("buttons", [])
    if not buttons:
        return None, None
    h = result.get("height", 1080)
    # Prefer buttons in the middle third vertically
    mid_third = [b for b in buttons if h // 3 < b["cy"] < 2 * h // 3]
    candidates = mid_third if mid_third else buttons
    # Pick largest by area
    best = max(candidates, key=lambda b: b["w"] * b["h"])
    return best["cx"], best["cy"]


# ── main ─────────────────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser(description="Local screenshot vision analysis")
    p.add_argument("--path", required=True, help="Path to PNG screenshot")
    p.add_argument("--prompt", default="", help="Ignored — structured output only")
    p.add_argument("--find-app", default="", help="Check if named app is foreground; outputs 'yes'/'no'")
    p.add_argument("--find-main-button", action="store_true",
                   help="Output 'cx cy' of the best button to click")
    args = p.parse_args()

    try:
        result = analyze(args.path)
    except Exception as e:
        result = {"error": str(e)}

    if args.find_app:
        sys.stdout.write(find_app(result, args.find_app) + "\n")
        return

    if args.find_main_button:
        cx, cy = find_main_button(result)
        if cx is not None:
            sys.stdout.write(f"{cx} {cy}\n")
        else:
            sys.stdout.write("\n")
        return

    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
