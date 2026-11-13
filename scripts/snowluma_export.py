#!/usr/bin/env python3
"""Snapshot the SnowLuma message store and export it as schema v1 history records (#58).

    python -I scripts/snowluma_export.py snapshot --live LIVE_DB --copy COPY_DB
    python -I scripts/snowluma_export.py export --db COPY_DB --out OUT.jsonl [--conv group:<id> ...]

snapshot opens the live store read-only (mode=ro, query_only) and copies it with SQLite's online
backup, which includes committed WAL pages; it refuses when the live -wal or -shm is missing (a
read-only open would create one beside the production database) and fails if the live
directory's file names change. export reads only the copy and writes records ordered by time.
Output stays on the Windows host; strip it before anything reaches the vault.
"""
import argparse
import json
import os
from pathlib import Path
import re
import sqlite3
from collections import Counter
import sys

REQUIRED = ('message_hash', 'is_group', 'session_id', 'sequence', 'private_direction', 'timestamp', 'data')
MEDIA = {'image': 'image', 'video': 'video', 'record': 'voice', 'file': 'file', 'flash_file': 'file',
         'mface': 'sticker', 'json': 'card', 'xml': 'card', 'markdown': 'markdown'}
CQ = re.compile(r'\[CQ:([A-Za-z_]+)((?:,[^,\]]*)*)\]')


def unescape(text):
    return text.replace('&#44;', ',').replace('&#91;', '[').replace('&#93;', ']').replace('&amp;', '&')


def parse_cq(message):
    items, at = [], 0
    for match in CQ.finditer(message):
        if match.start() > at:
            items.append({'type': 'text', 'data': {'text': unescape(message[at:match.start()])}})
        data = {}
        for part in match.group(2).split(',')[1:]:
            name, _, value = part.partition('=')
            data[name] = unescape(value)
        items.append({'type': match.group(1), 'data': data})
        at = match.end()
    if at < len(message):
        items.append({'type': 'text', 'data': {'text': unescape(message[at:])}})
    return items


def id_text(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str) and re.fullmatch(r'-?[0-9]+', value.strip()):
        return value.strip()
    return None


def segments(message):
    if isinstance(message, str):
        message = parse_cq(message)
    segs = []
    for item in message if isinstance(message, list) else []:
        if not isinstance(item, dict):
            continue
        kind, data = item.get('type'), item.get('data') if isinstance(item.get('data'), dict) else {}
        if kind == 'text':
            segs.append({'k': 'text', 'text': str(data.get('text', ''))})
        elif kind == 'at':
            segs.append({'k': 'at', 'id': 'all' if str(data.get('qq')) == 'all' else id_text(data.get('qq'))})
        elif kind == 'reply':
            segs.append({'k': 'reply', 'mid': id_text(data.get('id'))})
        elif kind in ('forward', 'node'):
            segs.append({'k': 'forward'})
        elif kind == 'face':
            segs.append({'k': 'face', 'id': id_text(data.get('id')) or ''})
        else:
            segs.append({'k': 'media', 'kind': MEDIA.get(kind, 'other')})
    return segs


def normalise(row, event):
    message_hash, is_group, session_id, private_direction, timestamp = row
    sender = event.get('sender') if isinstance(event.get('sender'), dict) else {}
    sender_id = id_text(event.get('user_id')) or id_text(sender.get('user_id'))
    if not is_group and private_direction in (0, 1):
        out = private_direction == 1
    else:
        out = event.get('post_type') == 'message_sent' or (sender_id is not None and sender_id == id_text(event.get('self_id')))
    seconds = timestamp if isinstance(timestamp, int) and timestamp > 0 else event.get('time')
    t = int(seconds) * 1000 if isinstance(seconds, (int, float)) and not isinstance(seconds, bool) and seconds > 0 else None
    return {'v': 1, 'src': 'snowluma', 'conv': f"{'group' if is_group else 'private'}:{session_id}",
            'dir': 'out' if out else 'in',
            'sender': {'id': sender_id, 'card': str(sender.get('card') or ''), 'nick': str(sender.get('nickname') or '')}
                      if sender_id else None,
            'convName': str(event['group_name']) if is_group and event.get('group_name') else None,
            't': t, 'mid': id_text(event.get('message_id')) or id_text(message_hash),
            'segs': segments(event.get('message'))}


def snapshot(live, copy):
    live, copy = Path(live), Path(copy)
    if copy.exists():
        print('snapshot refused: copy exists')
        return 2
    for sidecar in (live.with_name(live.name + '-wal'), live.with_name(live.name + '-shm')):
        if not sidecar.is_file():
            print('snapshot refused: live store has no -wal or -shm beside it')
            return 2
    before = sorted(os.listdir(live.parent))
    src = sqlite3.connect(live.resolve().as_uri() + '?mode=ro', uri=True)
    dst = None
    try:
        src.execute('PRAGMA query_only=1')
        dst = sqlite3.connect(copy)
        src.backup(dst)
        dst.execute('PRAGMA journal_mode=DELETE')
    except sqlite3.Error:
        if dst is not None:
            dst.close()
            dst = None
        for leftover in (copy, copy.with_name(copy.name + '-wal'), copy.with_name(copy.name + '-shm')):
            if leftover.exists():
                leftover.unlink()
        print('snapshot failed: the live store could not be read')
        return 1
    finally:
        src.close()
        if dst is not None:
            dst.close()
    after = sorted(os.listdir(live.parent))
    if after != before:
        print('live directory file set changed during snapshot')
        return 1
    check = sqlite3.connect(copy)
    try:
        count = check.execute('SELECT COUNT(*) FROM messages').fetchone()[0]
    finally:
        check.close()
    print(f'snapshot: messages={count}')
    return 0


def export(db, out, convs):
    out = Path(out)
    if out.exists():
        print('export refused: output exists')
        return 2
    try:
        conn = sqlite3.connect(Path(db).resolve().as_uri() + '?mode=ro', uri=True)
    except sqlite3.Error:
        print('export refused: the store cannot be opened read-only')
        return 2
    try:
        cols = {r[1] for r in conn.execute('PRAGMA table_info(messages)')}
        missing = [name for name in REQUIRED if name not in cols]
        if missing:
            print('export refused: missing columns ' + ', '.join(missing))
            return 2
        rows = conn.execute('SELECT message_hash, is_group, session_id, private_direction, timestamp, data '
                            'FROM messages ORDER BY timestamp, sequence, message_hash').fetchall()
    finally:
        conn.close()
    counts = Counter()
    records = []
    for row in rows:
        counts['rows'] += 1
        if row[5] is None:
            counts['no_data'] += 1
            continue
        try:
            event = json.loads(row[5])
        except json.JSONDecodeError:
            event = None
        if not isinstance(event, dict):
            counts['bad_json'] += 1
            continue
        record = normalise(row[:5], event)
        if convs and record['conv'] not in convs:
            counts['filtered'] += 1
            continue
        records.append(record)
    partial = out.with_name(out.name + '.partial')
    try:
        with open(partial, 'w', encoding='utf-8') as handle:
            for record in records:
                handle.write(json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n')
        os.replace(partial, out)
    except OSError:
        if partial.exists():
            partial.unlink()
        print('export failed: the output could not be written')
        return 1
    print(f"export: rows={counts['rows']} written={len(records)} no_data={counts['no_data']} "
          f"bad_json={counts['bad_json']} filtered={counts['filtered']} conversations={len({r['conv'] for r in records})} "
          f"in={sum(r['dir'] == 'in' for r in records)} out={sum(r['dir'] == 'out' for r in records)}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description='Snapshot and export the SnowLuma message store (#58).')
    commands = parser.add_subparsers(dest='command', required=True)
    take = commands.add_parser('snapshot', help='copy the live store with the online backup API')
    take.add_argument('--live', required=True)
    take.add_argument('--copy', required=True)
    dump = commands.add_parser('export', help='write schema v1 records from a copied store')
    dump.add_argument('--db', required=True)
    dump.add_argument('--out', required=True)
    dump.add_argument('--conv', action='append', default=[], help='keep only this conversation, group:<id> or private:<id>')
    args = parser.parse_args(argv)
    if args.command == 'snapshot':
        return snapshot(args.live, args.copy)
    return export(args.db, args.out, args.conv)


if __name__ == '__main__':
    sys.exit(main())
