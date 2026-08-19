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

# Порог домена — синхронно с crates/core/src/types.rs::SHORT_MAX_SECONDS.
SHORT_MAX_SECONDS = 90.0


def _domain(duration):
    """short (≤90с) / long (>90с) / unknown — как Domain::for_duration в ядре."""
    if duration is None:
        return "unknown"
    return "short" if duration <= SHORT_MAX_SECONDS else "long"


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

    DOMAINS = ("short", "long", "unknown")
    # счётчики: общие и по доменам
    cnt = {d: 0 for d in DOMAINS}
    dur_sum = {d: 0.0 for d in DOMAINS}
    totals = {d: {h: 0.0 for h in heights} for d in DOMAINS}
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
        dur = _num(meta.get("duration"))
        dom = _domain(dur)
        cnt[dom] += 1
        if dur:
            dur_sum[dom] += dur
        for h in heights:
            size, s = estimate(meta, h)
            if size is None:
                src[h]["none"] += 1
            else:
                totals[dom][h] += size
                src[h][s] += 1

    gb = 1024.0 ** 3
    n = sum(cnt.values())
    total_dur = sum(dur_sum.values())

    def line(label, c, dsum, tot):
        parts = [f"{label:<8} видео={c:<5}  длит={dsum / 3600:6.1f}ч"]
        for h in heights:
            parts.append(f"{h}p ~{tot[h] / gb:6.2f}GB")
        return "  ".join(parts)

    print(f"Всего видео (raw meta): {n}")
    print(f"Суммарная длительность: {total_dur / 3600:.1f} ч ({total_dur / 60:.0f} мин)")
    print()
    print("Оценка объёма скачивания (видео+аудио), по доменам:")
    for d in DOMAINS:
        if cnt[d]:
            print("  " + line(d, cnt[d], dur_sum[d], totals[d]))
    # ИТОГО по всем доменам
    all_tot = {h: sum(totals[d][h] for d in DOMAINS) for h in heights}
    print("  " + "-" * 60)
    print("  " + line("ИТОГО", n, total_dur, all_tot))
    print()
    print("Источник оценки по высотам (format=точно / bitrate=прикидка / none=нет данных):")
    for h in heights:
        c = src[h]
        print(f"  {h:>4}p:  по формату {c['format']}, по битрейту {c['bitrate']}, нет данных {c['none']}")
    print()
    print("Прим.: где нет filesize — tbr×duration, где нет и его — типовой битрейт H.264.")
    print("Реальный размер обычно в пределах ±20-30%. Домен: short ≤90с, long >90с.")


if __name__ == "__main__":
    main()
