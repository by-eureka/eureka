#!/usr/bin/env python3
"""Read harness traces as a snapshot, JSON, or a live terminal dashboard.

    python3 harness/trace_report.py runs/mine/trace.jsonl
    python3 harness/trace_report.py runs/*/trace.jsonl --live
    python3 harness/trace_report.py runs/*/trace.jsonl --json

Live/color output uses optional Rich (harness/requirements.txt). Plain text and
JSON need only Python. NO_COLOR disables styling. This only reads traces: an
unfinished attempt may still be running, or may have died without an end event.
"""
import argparse
from collections import Counter
import json
import math
import os
from pathlib import Path
import sys
import time


STATUS_STYLES = {
    'verified': 'bold green', 'failed': 'bold red', 'cleanup_failed': 'bold red',
    'rejected': 'red', 'not_submitted': 'yellow', 'finished': 'blue', 'reported': 'blue',
    'unfinished': 'cyan', 'verifying': 'cyan',
}


class TraceReader:
    """Keep aggregate state, not the conversation, and tail complete JSONL lines.

    Each reader owns one file's attempt IDs. Polling again must not count old
    events twice. A partial final line is retried at the same offset next time;
    replacement or truncation starts a fresh aggregate. No open file is retained.
    """

    def __init__(self, path):
        self.path = Path(path)
        self.identity = None
        self.offset = 0
        self.attempts = {}
        self.malformed = 0
        self.retries = Counter()
        self.tools = Counter()

    def read(self):
        partial = 0
        error = None
        try:
            with self.path.open('rb') as stream:
                stat = os.fstat(stream.fileno())
                identity = (stat.st_dev, stat.st_ino)
                if identity != self.identity or stat.st_size < self.offset:
                    self.__init__(self.path)
                    self.identity = identity
                stream.seek(self.offset)
                while line := stream.readline():
                    if not line.endswith(b'\n'):
                        partial = 1
                        break
                    self.offset = stream.tell()
                    try:
                        event = json.loads(line.decode('utf-8', errors='replace'))
                    except json.JSONDecodeError:
                        self.malformed += 1
                        continue
                    if not isinstance(event, dict):
                        self.malformed += 1
                        continue
                    self.consume(event)
        except OSError as exc:
            # A live view may start before its trace exists. Keep the other runs
            # visible, report the read error, and try this file again next refresh.
            error = str(exc)
        now = time.time() * 1000
        rows = []
        for saved in self.attempts.values():
            row = saved.copy()
            row['seconds_since_event'] = round(max(0, now - row.pop('last_ms')) / 1000)
            rows.append(row)
        report = dict(trace=str(self.path), attempts=rows, retries=dict(self.retries),
                      tools=dict(self.tools), malformed_lines=self.malformed + partial)
        if error:
            report['error'] = error
        return report

    def consume(self, event):
        name = event.get('attempt', '?')
        if name == 'harness':
            return  # Run-level usage would double-count these same attempts.
        row = self.attempts.setdefault(name, dict(
            id=name, label=name, status='unfinished', turns=0, tool_calls=0,
            compactions=0, input=0, cached=0, output=0, errors=0, incomplete=0))
        kind = event.get('event')
        row['last_ms'] = event.get('ts_ms', 0)
        if kind == 'attempt_start':
            row['label'] = event.get('label', name)
            row['artifacts'] = event.get('artifacts')
        elif kind in ('model_start', 'tool_start', 'compaction_start'):
            row['active'] = 'compaction' if kind == 'compaction_start' else event.get('name', 'model')
        elif kind in ('model', 'compaction_model'):
            row['active'] = None
            if kind == 'model':
                row['turns'] = event.get('turn', row['turns'] + 1)
            row['incomplete'] += bool(event.get('incomplete'))
            usage = event.get('usage', {})
            # Cumulative completed-turn usage, not context size. Cached is in input.
            for column, field in [('input', 'input_tokens'), ('cached', 'cached_input_tokens'),
                                  ('output', 'output_tokens')]:
                row[column] += usage.get(field, 0)
        elif kind == 'compaction':
            row['compactions'] += 1
            row['active'] = None
        elif kind == 'tool':
            row['active'] = None
            row['tool_calls'] += 1
            self.tools[event.get('name', '?')] += 1
            # Shell nonzero exits and Lean diagnostics can be ordinary tool text.
            row['errors'] += bool(event.get('error') or str(event.get('output', '')).startswith('ERROR:'))
        elif kind == 'request_retry':
            self.retries[event.get('reason', '?')[:160]] += 1
        elif kind == 'verification_start':
            row['status'] = 'verifying'
            row['active'] = 'Lean verifier'
        elif kind == 'attempt_end':
            row['status'] = event.get('status', event.get('verdict', 'finished'))
            row['active'] = None
            for field in ('candidate', 'note', 'termination', 'detail'):
                row[field] = event.get(field)


def summarize(path):
    """One-shot API; live mode reuses TraceReader instances between refreshes."""
    return TraceReader(path).read()


def print_plain(reports):
    """Keep the detailed text view useful in pipes and without Rich installed."""
    for report in reports:
        print(report['trace'])
        if report.get('error'):
            print(f"  cannot read trace: {report['error']}")
        for row in report['attempts']:
            active = f"; waiting on {row['active']}" if row.get('active') else ''
            print(f"  {row['id']} ({row['label']}): {row['status']}; turns={row['turns']}; "
                  f"compactions={row['compactions']}; "
                  f"tokens={row['input']}/{row['cached']}/{row['output']} in/cached/out; "
                  f"tool calls={row['tool_calls']}; tool errors={row['errors']}; "
                  f"incomplete={row['incomplete']}{active}; last event {row['seconds_since_event']}s ago")
            if row.get('candidate'):
                print(f"    candidate: {row['candidate']}")
        print(f"  retries: {sum(report['retries'].values())}; malformed/partial lines: {report['malformed_lines']}")


def dashboard(reports, width=120):
    """Render the same aggregates as JSON. Trace text is literal, never markup."""
    from rich.console import Group
    from rich.table import Table
    from rich.text import Text

    # At laptop widths keep status and progress readable; exact input/cache totals
    # remain available in the wide view, plain output, and JSON.
    narrow = width < 110
    table = Table(title='Eureka · proof runs', header_style='bold', box=None, padding=(0, 1))
    table.add_column('Run / agent', max_width=20 if narrow else 40, no_wrap=True, overflow='ellipsis')
    table.add_column('Status', min_width=10, no_wrap=True, overflow='ellipsis')
    for name in ('Turn', 'Cmp', 'Tools', 'Output' if narrow else 'Input / cached / output', 'Err'):
        table.add_column(name, justify='right', min_width=len(name), no_wrap=True)
    table.add_column('Waiting on', max_width=12 if narrow else 24, no_wrap=True, overflow='ellipsis')
    table.add_column('Age', justify='right', min_width=4, no_wrap=True)
    common = Path(os.path.commonpath([Path(r['trace']).resolve().parent for r in reports]))
    totals = Counter()
    notes = []
    for report in reports:
        path = Path(report['trace']).resolve()
        label = str(path.parent.relative_to(common)) if path.parent != common else path.parent.name
        if path.name != 'trace.jsonl':
            label += '/' + path.name
        totals['verified'] += any(row['status'] == 'verified' for row in report['attempts'])
        totals['retries'] += sum(report['retries'].values())
        totals['malformed'] += report['malformed_lines']
        if report.get('error'):
            notes.append(Text(f"{label}: {report['error']}", style='yellow'))
        if not report['attempts']:
            table.add_row(Text(label), Text('awaiting trace', style='yellow'))
        for row in report['attempts']:
            totals['agents'] += 1
            totals['errors'] += row['errors']
            totals['incomplete'] += row['incomplete']
            tokens = ' / '.join(f"{row[k]:,}" for k in (('output',) if narrow else ('input', 'cached', 'output')))
            number = row['id'].removeprefix('attempt-').lstrip('0') or '0'
            table.add_row(
                Text(f"{label}/{row['label']} #{number}"),
                Text(row['status'], style=STATUS_STYLES.get(row['status'], 'dim')),
                str(row['turns']), str(row['compactions']), str(row['tool_calls']), tokens,
                Text(str(row['errors']), style='bold red' if row['errors'] else 'dim'),
                Text(row.get('active') or '—', style='cyan'),
                Text(f"{row['seconds_since_event']}s", style='dim'))
    summary = Text(f"{totals['verified']}/{len(reports)} verified · {totals['agents']} agents · "
                   f"{totals['errors']} tool errors · {totals['retries']} retries · "
                   f"{totals['incomplete']} incomplete · {totals['malformed']} partial/bad lines")
    note = Text('Trace state only; quiet does not mean dead. Cached tokens are included in input.\n'
                'Ctrl-C exits the live view. Use --json for exact counters and file paths.', style='dim')
    return Group(table, summary, *notes, note)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('traces', type=Path, nargs='+')
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--json', action='store_true')
    mode.add_argument('--live', action='store_true', help='refresh the dashboard until Ctrl-C')
    parser.add_argument('--interval', type=float, default=5, help='live refresh interval in seconds (default: 5)')
    parser.add_argument('--color', choices=('auto', 'always', 'never'), default='auto')
    args = parser.parse_args()
    if not math.isfinite(args.interval) or args.interval <= 0:
        parser.error('--interval must be a positive finite number')
    if args.live and (not sys.stdout.isatty() or os.getenv('TERM') == 'dumb'):
        parser.error('--live needs a terminal; omit it for a snapshot or use --json')
    readers = [TraceReader(path) for path in args.traces]
    reports = [reader.read() for reader in readers]
    if args.json:
        print(json.dumps(reports, indent=2))
        return
    if not args.live and not sys.stdout.isatty() and args.color != 'always':
        print_plain(reports)
        return
    try:
        from rich.console import Console
        from rich.live import Live
        from rich.text import Text
    except ImportError:
        if args.live or args.color == 'always':
            parser.error('install the dashboard dependency: python3 -m pip install -r harness/requirements.txt')
        print_plain(reports)
        return
    console = Console(force_terminal=True if args.color == 'always' else None,
                      color_system=None if args.color == 'never' or 'NO_COLOR' in os.environ else 'auto')
    if not args.live:
        console.print(dashboard(reports, console.width))
        return
    # Rich owns screen restoration and resizing. One refresh loop owns file reads;
    # there is no background polling thread or second parser for the live display.
    with Live(dashboard(reports, console.width), console=console, screen=True, auto_refresh=False) as live:
        try:
            while True:
                time.sleep(args.interval)
                reports = [reader.read() for reader in readers]
                live.update(dashboard(reports, console.width), refresh=True)
        except KeyboardInterrupt:
            pass
    console.print(Text('Monitor stopped. Harness runs are unchanged.', style='dim'))


if __name__ == '__main__':
    main()
