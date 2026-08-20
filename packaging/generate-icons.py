"""Build Longwatch application icons from the approved OpenAI mark."""

from __future__ import annotations

import argparse
from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
PACKAGING = ROOT / "packaging"
MARK_SOURCE = PACKAGING / "openai-mark.png"
MASTER_SIZE = 1024
UI_SIZE = 256
TOAST_SIZE = 256
RESAMPLE = Image.Resampling.LANCZOS


def extracted_alpha(source_path: Path) -> Image.Image:
    with Image.open(source_path) as source:
        image = source.convert("RGBA")
    alpha = image.getchannel("A")
    bounds = alpha.getbbox()
    if bounds is None:
        raise ValueError(f"logo source has no visible pixels: {source_path}")
    return alpha.crop(bounds)


def fitted_alpha(alpha: Image.Image, maximum: int) -> Image.Image:
    scale = min(maximum / alpha.width, maximum / alpha.height)
    target = (
        max(1, round(alpha.width * scale)),
        max(1, round(alpha.height * scale)),
    )
    return alpha.resize(target, RESAMPLE)


def save_mark_source(alpha: Image.Image) -> None:
    fitted = fitted_alpha(alpha, 900)
    mark = Image.new("RGBA", (MASTER_SIZE, MASTER_SIZE), (0, 0, 0, 0))
    mark_layer = Image.new("RGBA", fitted.size, (0, 0, 0, 255))
    mark_layer.putalpha(fitted)
    position = ((MASTER_SIZE - fitted.width) // 2, (MASTER_SIZE - fitted.height) // 2)
    mark.alpha_composite(mark_layer, position)
    mark.save(MARK_SOURCE, format="PNG", optimize=True)


def branded_master(
    alpha: Image.Image,
    background_color: tuple[int, int, int, int],
    mark_color: tuple[int, int, int, int],
    border_color: tuple[int, int, int, int],
) -> Image.Image:
    canvas = Image.new("RGBA", (MASTER_SIZE, MASTER_SIZE), (0, 0, 0, 0))
    rounded_mask = Image.new("L", (MASTER_SIZE, MASTER_SIZE), 0)
    ImageDraw.Draw(rounded_mask).rounded_rectangle(
        (10, 10, MASTER_SIZE - 10, MASTER_SIZE - 10),
        radius=230,
        fill=255,
    )

    background = Image.new("RGBA", (MASTER_SIZE, MASTER_SIZE), background_color)
    canvas.paste(background, (0, 0), rounded_mask)

    fitted = fitted_alpha(alpha, 704)
    position = ((MASTER_SIZE - fitted.width) // 2, (MASTER_SIZE - fitted.height) // 2)
    mark = Image.new("RGBA", fitted.size, mark_color)
    mark.putalpha(fitted)
    canvas.alpha_composite(mark, position)

    ImageDraw.Draw(canvas).rounded_rectangle(
        (11, 11, MASTER_SIZE - 12, MASTER_SIZE - 12),
        radius=229,
        outline=border_color,
        width=4,
    )
    return canvas


def save_icns(master: Image.Image, destination: Path) -> None:
    icns_images = [
        master.resize((size, size), RESAMPLE)
        for size in (16, 32, 64, 128, 256, 512, 1024)
    ]
    master.save(destination, format="ICNS", append_images=icns_images)


def toast_mark(
    alpha: Image.Image,
    mark_color: tuple[int, int, int, int],
) -> Image.Image:
    canvas = Image.new("RGBA", (TOAST_SIZE, TOAST_SIZE), (0, 0, 0, 0))
    fitted = fitted_alpha(alpha, 196)
    mark = Image.new("RGBA", fitted.size, mark_color)
    mark.putalpha(fitted)
    position = ((TOAST_SIZE - fitted.width) // 2, (TOAST_SIZE - fitted.height) // 2)
    canvas.alpha_composite(mark, position)
    return canvas


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--source",
        type=Path,
        default=MARK_SOURCE,
        help="approved transparent OpenAI mark; defaults to packaging/openai-mark.png",
    )
    args = parser.parse_args()
    source_path = args.source.expanduser().resolve()
    if not source_path.is_file():
        raise FileNotFoundError(f"logo source does not exist: {source_path}")

    mark_alpha = extracted_alpha(source_path)
    save_mark_source(mark_alpha)
    master = branded_master(
        mark_alpha,
        background_color=(255, 255, 255, 255),
        mark_color=(32, 33, 35, 255),
        border_color=(32, 33, 35, 38),
    )
    macos_dark_master = branded_master(
        mark_alpha,
        background_color=(32, 33, 35, 255),
        mark_color=(255, 255, 255, 255),
        border_color=(255, 255, 255, 46),
    )
    png_path = PACKAGING / "icon.png"
    ui_path = PACKAGING / "ui-logo.png"
    ico_path = PACKAGING / "windows" / "Longwatch.ico"
    toast_light_path = PACKAGING / "windows" / "toast-light.png"
    toast_dark_path = PACKAGING / "windows" / "toast-dark.png"
    light_icns_path = PACKAGING / "macos" / "Longwatch-Light.icns"
    dark_icns_path = PACKAGING / "macos" / "Longwatch-Dark.icns"

    master.save(png_path, format="PNG", optimize=True)
    master.resize((UI_SIZE, UI_SIZE), RESAMPLE).save(ui_path, format="PNG", optimize=True)
    master.save(
        ico_path,
        format="ICO",
        sizes=[
            (16, 16),
            (20, 20),
            (24, 24),
            (32, 32),
            (40, 40),
            (48, 48),
            (64, 64),
            (128, 128),
            (256, 256),
        ],
        bitmap_format="png",
    )
    toast_mark(mark_alpha, (32, 33, 35, 255)).save(
        toast_light_path, format="PNG", optimize=True
    )
    toast_mark(mark_alpha, (255, 255, 255, 255)).save(
        toast_dark_path, format="PNG", optimize=True
    )
    save_icns(master, light_icns_path)
    save_icns(macos_dark_master, dark_icns_path)

    print(f"Imported {source_path}")
    print(f"Generated {MARK_SOURCE}")
    print(f"Generated {png_path}")
    print(f"Generated {ui_path}")
    print(f"Generated {ico_path}")
    print(f"Generated {toast_light_path}")
    print(f"Generated {toast_dark_path}")
    print(f"Generated {light_icns_path}")
    print(f"Generated {dark_icns_path}")


if __name__ == "__main__":
    main()
