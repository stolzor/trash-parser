#!/usr/bin/env python3
"""Оценка объёма медиа по собранным Bronze-метаданным (сырой yt-dlp `-J`).

Читает `raw/meta/*.json` из каталога и оценивает суммарный размер скачивания
для выбранных высот (по умолчанию 240p и 360p), беря на каждое видео:
  1) filesize / filesize_approx нужного формата, иначе
  2) tbr × duration (битрейт формата × длительность), иначе
  3) типовой битрейт H.264 (грубый фолбэк).
Видео-only форматы дополняются самой лёгкой аудио-дорожкой (как сделал бы
селектор `bestvideo[height<=H]+bestaudio`).

Использование:
    python3 scripts/estimate_media.py <каталог-с-json> [высоты...]
    python3 scripts/estimate_media.py /tmp/detox-meta 240 360
"""
import json
import os
import sys

# Типовой суммарный (видео+аудио) битрейт H.264 в кбит/с — фолбэк, если у формата
# нет ни размера, ни tbr. Значения консервативные (реальность ±20-30%).
FALLBACK_KBPS = {144: 120, 240: 320, 360: 650, 480: 1100, 720: 2500, 1080: 4500}


def _num(x):
    return x if isinstance(x, (int, float)) and not isinstance(x, bool) else None


def _fmt_size(f, duration):
    """Размер формата в байтах: filesize → filesize_approx → tbr×duration → None."""
    for k in ("filesize", "filesize_approx"):
        v = _num(f.get(k))
        if v:
            return float(v)
    tbr = _num(f.get("tbr"))
    if tbr and duration:
        return tbr * 1000.0 / 8.0 * duration  # кбит/с → байт/с × сек
    return None


def _has_video(f):
    return f.get("vcodec") not in (None, "none")


def _has_audio(f):
    return f.get("acodec") not in (None, "none")


def _pick_video(formats, target_h):
    """Видео-формат ближе всего к target_h снизу (иначе минимальный доступный)."""
    vids = [f for f in formats if _has_video(f) and _num(f.get("height"))]
    if not vids:
        return None
    le = [f for f in vids if f["height"] <= target_h]
    return max(le, key=lambda f: f["height"]) if le else min(vids, key=lambda f: f["height"])


def _best_audio(formats):
    """Самая лёгкая аудио-only дорожка (для детокс-фич качество звука не критично)."""
    auds = [f for f in formats if _has_audio(f) and not _has_video(f)]
    if not auds:
        return None
    return min(auds, key=lambda f: (_num(f.get("abr")) or _num(f.get("tbr")) or 1e9))


def estimate(meta, target_h):
    """(размер_в_байтах, источник) для одного видео при высоте target_h."""
    duration = _num(meta.get("duration"))
    formats = meta.get("formats") or []
    vf = _pick_video(formats, target_h)

    if vf is None:  # нет пригодных форматов → оценка по типовому битрейту
        if duration:
            kbps = FALLBACK_KBPS.get(target_h, 650)
            return kbps * 1000.0 / 8.0 * duration, "bitrate"
        return None, None

    vsize = _fmt_size(vf, duration)
    src = "format"
    if vsize is None:
        if duration:
            kbps = FALLBACK_KBPS.get(target_h, 650)
            vsize, src = kbps * 1000.0 / 8.0 * duration, "bitrate"
        else:
            return None, None

    asize = 0.0
    if not _has_audio(vf):  # видео-only → добавить аудио-дорожку
        af = _best_audio(formats)
        if af:
            asize = _fmt_size(af, duration) or 0.0
    return vsize + asize, src


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    root = sys.argv[1]
    heights = [int(h) for h in sys.argv[2:]] or [240, 360]

    files = [
        os.path.join(dp, fn)
        for dp, _, fns in os.walk(root)
        for fn in fns
        if fn.endswith(".json")
    ]

    n = n_dur = 0
    total_dur = 0.0
    totals = {h: 0.0 for h in heights}
    src = {h: {"format": 0, "bitrate": 0, "none": 0} for h in heights}

    for path in files:
        try:
            with open(path, encoding="utf-8") as fh:
                meta = json.load(fh)
        except Exception:
            continue
        # только сырой yt-dlp meta (есть duration/formats), не normalized (duration_s)
        if not isinstance(meta, dict) or ("formats" not in meta and "duration" not in meta):
            continue
        n += 1
        dur = _num(meta.get("duration"))
        if dur:
            n_dur += 1
            total_dur += dur
        for h in heights:
            size, s = estimate(meta, h)
            if size is None:
                src[h]["none"] += 1
            else:
                totals[h] += size
                src[h][s] += 1

    gb = 1024.0 ** 3
    print(f"Видео (raw meta):       {n}")
    print(f"  с длительностью:      {n_dur}")
    print(f"Суммарная длительность: {total_dur / 3600:.1f} ч ({total_dur / 60:.0f} мин)")
    if n_dur:
        print(f"Средняя длина:          {total_dur / n_dur:.0f} с")
    print()
    print("Оценка объёма скачивания (видео+аудио):")
    for h in heights:
        vol_gb = totals[h] / gb
        avg_mb = (totals[h] / n / (1024 * 1024)) if n else 0.0
        c = src[h]
        print(
            f"  {h:>4}p:  ~{vol_gb:6.2f} GB   (~{avg_mb:5.1f} MB/видео; "
            f"по формату: {c['format']}, по битрейту: {c['bitrate']}, нет данных: {c['none']})"
        )
    print()
    print("Прим.: оценка приблизительная. Где нет filesize — взят tbr×duration, где")
    print("нет и его — типовой битрейт H.264. Реальный размер обычно в пределах ±20-30%.")


if __name__ == "__main__":
    main()
