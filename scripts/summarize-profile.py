#!/usr/bin/env python3
"""Summarize Glimcode JSONL timing captures; no third-party dependencies."""
import argparse
import json
import math
from pathlib import Path


def percentile(values, quantile):
    values = sorted(values)
    index = (len(values) - 1) * quantile
    low, high = math.floor(index), math.ceil(index)
    return values[low] + (values[high] - values[low]) * (index - low)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    parser.add_argument('--skip-frames', type=int, default=0, help='Exclude startup frames explicitly; default includes all frames')
    args = parser.parse_args()
    records = []
    truncated = 0
    for line in args.trace.read_text().splitlines():
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            truncated += 1
    records = records[args.skip_frames:]
    print(f'Frames: {len(records)}; input frames: {sum(r.get("input_events", 0) > 0 for r in records)}; incomplete records: {truncated}')
    print('Metric (ms)                          p50      p95      p99      max   >8.33ms')
    for metric in ['app_cpu_ms', 'input_to_ui_ms', 'ui_to_render_callback_ms', 'input_to_render_callback_ms', 'ui_to_gpu_complete_ms', 'input_to_gpu_complete_ms']:
        values = [r[metric] for r in records if isinstance(r.get(metric), (int, float))]
        if not values:
            continue
        print(f'{metric:34} {percentile(values, .50):8.3f} {percentile(values, .95):8.3f} {percentile(values, .99):8.3f} {max(values):8.3f} {sum(v > 1000/120 for v in values):8}')
    continuous = [r['frame_interval_ms'] for r in records if r.get('continuous') and isinstance(r.get('frame_interval_ms'), (int, float))]
    if continuous:
        print(f'Continuous repaint intervals: p50={percentile(continuous, .50):.3f} ms, p99={percentile(continuous, .99):.3f} ms')
    rates = sorted({r['monitor_hz'] for r in records if r.get('monitor_hz')})
    print(f'Reported monitor refresh rates: {rates or "unavailable"}; dropped records: {max((r.get("dropped_records", 0) for r in records), default=0)}')
    print('Event-to-frame metrics exclude window swap and physical display scanout; terminal echo can arrive in a later frame.')
    if not any(r.get('input_events', 0) for r in records):
        print('No real input was captured; key latency remains unmeasured.')


if __name__ == '__main__':
    main()
